.PHONY: check ffmpeg ffmpeg_present \
	rs_check rs_fmt rs_lint rs_clippy rs_doc rs_test rs_cov \
	py_check py_env py_develop py_stubs py_test

FFMPEG_DIST := third_party/ffmpeg/dist

# macOS：bindgen 若加载的是不带默认 sysroot 的 libclang（如 Homebrew LLVM）会找不到系统头文件，显式指定 SDK
ifeq ($(shell uname -s),Darwin)
export BINDGEN_EXTRA_CLANG_ARGS := --sysroot=$(shell xcrun --show-sdk-path)
endif

# Python 开发环境：uv 把开发工具装进 $(VENV)（Python 版本由 UV_PYTHON 指定，如 make py_test VENV=.venv-3.15
# UV_PYTHON=3.15）；PyO3 的构建脚本按它确定 Python 版本与链接参数
VENV ?= .venv
export UV_PROJECT_ENVIRONMENT := $(VENV)
PYTHON := $(abspath $(VENV)/bin/python)
export PYO3_PYTHON := $(PYTHON)

check: rs_check py_check

# Rust 的全部检查；CI 按平台调用其中的子目标
rs_check: rs_fmt rs_lint rs_clippy rs_doc rs_test rs_cov

rs_fmt:
	cargo fmt --all --check

# 与平台无关的检查：依赖方向、压制属性、依赖的安全公告与许可证
rs_lint:
	scripts/check_deps.sh
	scripts/check_no_suppression.sh
	@cargo deny --version >/dev/null 2>&1 || { echo "缺少 cargo-deny：cargo install cargo-deny --version 0.20.2 --locked"; exit 1; }
	cargo deny --locked check

rs_clippy: ffmpeg_present py_env
	cargo clippy --workspace --all-targets -- -D warnings

# 文档注释里的链接须都能解析（含私有项）：改名、删除后留下的断链在这里失败
rs_doc: ffmpeg_present py_env
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

rs_test: ffmpeg_present py_env
	cargo test --workspace

# 数据正确性模块（hls、core、remux）的行覆盖各自不低于 80%
rs_cov: ffmpeg_present
	@cargo llvm-cov --version >/dev/null 2>&1 || { echo "缺少 cargo-llvm-cov：cargo install cargo-llvm-cov --version 0.9.1 --locked"; exit 1; }
	# 源码改动后旧的插桩产物会混进统计，先清掉
	cargo llvm-cov clean --workspace
	cargo llvm-cov -p hs-m3u8-hls --fail-under-lines 80 --summary-only
	cargo llvm-cov -p hs-m3u8-core --fail-under-lines 80 --summary-only
	cargo llvm-cov -p hs-m3u8-remux --fail-under-lines 80 --summary-only

ffmpeg_present:
	@test -d $(FFMPEG_DIST)/include || { echo "缺少 $(FFMPEG_DIST)，先运行 make ffmpeg"; exit 1; }

ffmpeg:
	third_party/ffmpeg/build.sh

py_env:
	uv sync --locked

# 构建扩展模块（调试版）并装进 .venv
py_develop: py_env ffmpeg_present
	uv run --no-sync maturin develop --uv

# 由 crates/py 的定义重新生成存根与 __init__.py。stub_gen 内嵌 Python 解释器：PYTHONHOME 指明标准库的位置，
# LD_LIBRARY_PATH 让 Linux 上找得到 libpython（uv 装的 Python 不在系统的库路径里）
py_stubs: py_env ffmpeg_present
	PYTHONHOME="$$($(PYTHON) -c 'import sys; print(sys.base_prefix)')" \
	LD_LIBRARY_PATH="$$($(PYTHON) -c 'import sysconfig; print(sysconfig.get_config_var("LIBDIR"))')" \
	cargo run --locked -q -p hs-m3u8-py --bin stub_gen

# 构建扩展模块并跑 Python 的端到端测试
py_test: py_develop
	uv run --no-sync pytest

# Python 绑定的全部检查
py_check: py_stubs py_develop
	@# 生成的文件须已提交或暂存：重新生成后与暂存区一致，且没有未纳入版本库的新文件
	git diff --exit-code -- python/hs_m3u8
	@test -z "$$(git ls-files --others --exclude-standard -- python/hs_m3u8)" || { git ls-files --others --exclude-standard -- python/hs_m3u8; echo "以上生成的文件未纳入版本库"; exit 1; }
	uv run --no-sync ruff format --check python tests/python
	uv run --no-sync ruff check python tests/python
	uv run --no-sync mypy
	uv run --no-sync basedpyright
	@# pyo3-stub-gen 不生成 @disjoint_base（PEP 800），而 PyO3 的类在运行时都是 disjoint base；只影响对多重继承原生类的检查
	uv run --no-sync python -m mypy.stubtest hs_m3u8 --ignore-disjoint-bases
	uv run --no-sync pytest
