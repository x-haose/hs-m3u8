.PHONY: sb publish_testpypi publish_pypi test release check py_check rs_check check_i ffmpeg

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

test: sb publish_testpypi

release: sb publish_pypi

check: py_check rs_check

py_check:
	uv run pre-commit run --all-files

rs_check:
	@test -d $(FFMPEG_DIST)/include || { echo "缺少 $(FFMPEG_DIST)，先运行 make ffmpeg"; exit 1; }
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

check_i:
	uv run pre-commit install

ffmpeg:
	third_party/ffmpeg/build.sh
