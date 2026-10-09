.PHONY: sb publish_testpypi publish_pypi release_testpypi release check py_check check_i ffmpeg \
	rs_check rs_fmt rs_lint rs_clippy rs_test rs_cov ffmpeg_present

FFMPEG_DIST := third_party/ffmpeg/dist

# macOS：bindgen 若加载的是不带默认 sysroot 的 libclang（如 Homebrew LLVM）会找不到系统头文件，显式指定 SDK
ifeq ($(shell uname -s),Darwin)
export BINDGEN_EXTRA_CLANG_ARGS := --sysroot=$(shell xcrun --show-sdk-path)
endif

sb:
	rm -rf ./dist
	uv build

publish_testpypi:
	uv run twine upload -r testpypi dist/*

publish_pypi:
	uv run twine upload dist/*

release_testpypi: sb publish_testpypi

release: sb publish_pypi

check: py_check rs_check

py_check:
	uv run pre-commit run --all-files

# Rust 的全部检查；CI 按平台调用其中的子目标
rs_check: rs_fmt rs_lint rs_clippy rs_test rs_cov

rs_fmt:
	cargo fmt --all --check

# 与平台无关的检查：依赖方向、压制属性、依赖的安全公告与许可证
rs_lint:
	scripts/check_deps.sh
	scripts/check_no_suppression.sh
	@cargo deny --version >/dev/null 2>&1 || { echo "缺少 cargo-deny：cargo install cargo-deny --version 0.20.2 --locked"; exit 1; }
	cargo deny --locked check

rs_clippy: ffmpeg_present
	cargo clippy --workspace --all-targets -- -D warnings

rs_test: ffmpeg_present
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

check_i:
	uv run pre-commit install

ffmpeg:
	third_party/ffmpeg/build.sh
