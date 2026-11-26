"""合并文件放不进的原因与异常的通用名字：合并文件可以是 MP4 或 MKV，两种都用它们；与 1.0 的名字是同一个类。"""

from hs_m3u8._native import Mp4Unsupported, Mp4UnsupportedError

MergeUnsupported = Mp4Unsupported
MergeUnsupportedError = Mp4UnsupportedError
