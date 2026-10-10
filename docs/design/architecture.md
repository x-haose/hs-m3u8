# hs-m3u8 Rust 版架构设计

- 状态：已采纳
- 日期：2026-10-09
- 相关决定：ADR-0001（技术路线）、ADR-0002（FFmpeg）、ADR-0003（仓库与迁移）、ADR-0004（产品范围）、ADR-0005（前端技术栈）、ADR-0006（播放列表解析）

## 1. 目标与非目标

目标：

- 下载 HLS 点播流并原样复制（不重编码）为单个 MP4、可直接播放的本地 HLS 目录，或两者都要：TS 与 fMP4 分片、AES-128 加密、字节范围分片、不连续段、视频与独立音频 rendition 合流。
- 正确性优先：每个分片使用自己的 key 与 IV；失败一律上抛；任何时刻中断都可安全续传；产物要么完整、要么不存在。
- 同一核心服务三类使用方：Rust 库、Python 包 `hs-m3u8`、Tauri 桌面应用。
- 站点适配通过回调完成（改写播放列表、修改请求、变换 key、变换分片）。
- 直播录制（无 `#EXT-X-ENDLIST` 的播放列表），中断后可续录，见 5.9。

非目标：

- DRM（Widevine、FairPlay、PlayReady）与 SAMPLE-AES：识别后明确报不支持。
- 转码。
- HTTP 直链、BT、磁力下载（ADR-0004）。
- 浏览器指纹模拟：v1 不做，以后作为可选功能加入（wreq 0.16.1 仍在 1.0 之前，要求 Rust 1.98）。

## 2. 仓库结构

```
hs-m3u8/
├── Cargo.toml              workspace
├── rust-toolchain.toml     锁定工具链版本
├── crates/
│   ├── hls/                播放列表解析与规范化、选轨、IV 推导；纯计算，无 IO
│   ├── core/               下载任务：HTTP、调度、解密校验、续传、直播录制、合并编排、进度
│   ├── remux/              FFmpeg 转封装；全项目唯一的 FFI 边界
│   └── py/                 PyO3 绑定（cdylib），模块名 hs_m3u8._native（Python 绑定阶段建立）
├── python/hs_m3u8/         Python 包：类型化包装、异常类、.pyi（同上）
├── apps/desktop/           Tauri 应用：src-tauri/ 与前端（GUI 阶段建立）
├── third_party/
│   ├── ffmpeg/             FFmpeg 构建脚本：固定版本与 configure 参数
│   └── ffmpeg-sys-next/    ffmpeg-sys-next 的本地修改版，差异见其中的 PATCHED.md
├── scripts/                检查脚本：依赖方向、禁止压制属性
├── tests/fixtures/         测试样本（用 ffmpeg testsrc 生成，不含第三方内容）
└── docs/
```

## 3. 分层与依赖方向

```
apps/desktop ─┐
crates/py ────┼──> crates/core ──> crates/hls
              │          └──────> crates/remux
```

- `hls` 不依赖 tokio、reqwest、ffmpeg；`remux` 不依赖 `core`、`hls`、tokio、reqwest；`core` 不依赖 tauri、pyo3。
- 跨边界一律翻译：`hls` 不暴露第三方解析库的类型；`py` 与 `apps/desktop` 把 `core` 的类型转成各自的表示，`core` 不出现 Python 或前端的概念。`core` 的公开接口只重新导出用到的 `hls` 类型（选轨偏好与解析错误，在 `core::hls` 下）与 `remux` 的合并报告和错误（在 `core::remux` 下），绑定层只依赖 `core`。
- 以上由检查命令机器判定（第 10 节），不靠人看。

## 4. hls：数据模型与规范化

只做纯计算：输入播放列表文本和它的最终 URL（跟随重定向之后），输出规范化模型。解析器自行实现，理由见 ADR-0006。

类型定义见 `crates/hls/src`（`MasterPlaylist`、`Variant`、`Rendition`、`MediaPlaylist`、`Segment`、`SegmentKey`、`InitSection`）。约定：所有 URI 均为绝对地址；时长为整数微秒；每个分片自带已定值的 key 与 IV、init 段、字节范围与不连续段序号。

规范化规则（RFC 8216，全部有纯计算测试）：

- `EXT-X-KEY` 作用于其后所有分片，直到下一个 `EXT-X-KEY`；`METHOD=NONE` 清除。缺省 IV 时用该分片媒体序号的 16 字节大端编码。
- 同一位置有多个 `EXT-X-KEY` 时，只取 `KEYFORMAT` 缺省或为 `identity` 的那个；只剩 DRM 的 KEYFORMAT 时报 `hls::Encryption::Drm`；`SAMPLE-AES` 报 `hls::Encryption::SampleAes`。
- `EXT-X-MAP` 作用于其后所有分片，直到下一个 `EXT-X-MAP`。
- `EXT-X-BYTERANGE` 省略偏移量时，接着同一资源上一个子区间往后取。
- 不连续段序号 = `EXT-X-DISCONTINUITY-SEQUENCE`（缺省 0）加上此前出现的 `EXT-X-DISCONTINUITY` 个数。
- 媒体序号、不连续段序号、字节范围的结束位置超出 64 位整数时报错。
- 所有 URI 按**该播放列表的最终 URL**（跟随重定向之后）解析。
- 解析错误分两类：内容不是合法的播放列表为 `Malformed`（为空 `Empty`、首行不是 `#EXTM3U` 为 `NotAPlaylist`、主与媒体播放列表的标签混在一起为 `Mixed`、带行号的 `Syntax`），用了不支持的加密为 `Unsupported`（带 `hls::Encryption`）。

选轨（`select`）：

- 默认取最高分辨率，同分辨率取最高带宽；有带 `RESOLUTION` 的变体时，不考虑没有它的（多为纯音频）；也可按下标指定。
- 变体带 `AUDIO` 组、且组内 rendition 有 URI 时，按指定的语言或第几个音频 rendition（只数音频，同一语言有多条时只能这样区分）选，未指定时选 `DEFAULT=YES` 的，没有则取组内第一个；这就是音频独立成一条媒体播放列表的情况。此时只取变体的视频，变体里即使混有音频也不用，与播放器的行为一致。

## 5. core：下载任务

### 5.1 任务与流水线

一个任务把一个来源 URL 变成它的输出，分三条流程，共用写出与收尾：

- **点播**：解析（取主播放列表、选轨、取媒体播放列表）→ 计划（各轨分片、不连续段组、计划摘要）→ 拉取 init 段并确认能够合并 → 下载分片 → 合并。
- **直播录制**：解析 → 按 5.9 录制 → 合并任务目录中已录到的分片。
- **直播只合并**：不联网，直接合并任务目录中已录到的分片（`Engine::merge_recorded`，与下载共用输出配置 `OutputOptions`，不需要来源；与 `start` 一样返回任务句柄，有进度、写出前可取消）；点播的下载报 `WorkDir(NotLiveRecording)`，结果里没有录制结束原因。

输出（`Target`）为 MP4、可直接播放的本地 HLS 目录，或两者都要，内容相同：只放各轨都有的不连续段组。先把 MP4 合并到临时名（`remux` 写完核对包数并落盘），再在准备目录中备齐 HLS（能硬链接就硬链接，做不了时复制并落盘，不写穿已有文件），都成功后换上：只挪开检查认可替换的东西（要求覆盖时的旧输出，或空目录），装上新的，全部装上后在记录里记为已换好，再删旧的（删不掉的记在结果的 `leftovers`）。装上与放回都不替换已有的（MP4 用硬链接；文件系统不支持硬链接时只能改名，改名到空目录时替换它），几个任务同时写同一输出，先装上的成功，后到的报已存在。任一步失败都撤回，每一项尽量做完；最常见的失败（编码不受支持）发生在合并 MP4 时，还没有动到输出。临时名与挪开旧输出的名字与输出同级，为定长的 `hsdl-<任务目录指纹>.<mp4|hls>.part` 与 `.old`，按任务目录保留：同一任务目录同一时间只有一个任务，这两个名字上的东西不看内容就收拾——临时输出删掉；挪开的旧输出，换上还没做完的，原处空着或是空目录就放回、原处是输出就删掉（只有要替换时才挪开，原处是输出说明更新的输出已装上）、原处是别的东西就留着报出来；已换好的与没有记录的（换好后删不掉而留下）只删不放回。写之前把各输出记进任务目录的 `outputs.json`；每次运行在任务开头、联网与检查输出之前（任务目录已存在时先加锁），按记录与本次各输出的名字收拾一遍，没能放回的旧输出原处是别的东西时任务在这里失败（`Cleanup`），要用户处理。之后按选项删除或保留任务目录。

本地 HLS 不经 FFmpeg，原样保留已解密的分片，不受 MP4 合并对编码的限制。入口 `index.m3u8` 单轨时为媒体播放列表，音视频分离时为主播放列表（码率为两条轨分片峰值码率之和，按落盘的文件算，某条轨的分片声明的时长都为 0 算不出时用来源写的，来源也没写报 `Unsupported::Hls(BandwidthUnknown)`，合并 MP4 之前就判定；分辨率与编码照抄来源）；各轨的分片在 `<轨道>/` 下按播放顺序编号。扩展名按内容定（TS、AAC、MP3、AC-3、E-AC-3，fMP4 为 `m4s`）：FFmpeg 读 HLS 时核对扩展名与识别出的格式，不一致即拒绝（FFmpeg 9.0.2 的 `extension_picky`，默认开启）。组与组之间加 `EXT-X-DISCONTINUITY`；组内缺失的分片不标出、保留原时间戳，与 MP4 一致：缺失往往只在一条轨上，只给一条轨加不连续标记会让各轨的不连续段编号对不上。`EXT-X-MAP` 一直作用到下一个 `EXT-X-MAP`，同一条轨里没有 init 段的组排在有的组后面（直播续录时服务器从 fMP4 换成 TS）无法表示，合并 MP4 之前就报 `Unsupported::Hls(MixedInit)`；先 TS 后 fMP4 照常写出。

开始下载前可用 `Engine::probe` 只解析不下载：请求与回调同下载，返回可选的变体与音轨（带下标，按下标选用）、按偏好选中的轨、各轨概况（分片数、时长、是否结束、是否加密）与是否直播。探测结果与进度里的所选轨道都是 core 自己的描述类型，不含地址：地址常带令牌，而它们常被打进日志与界面。来源与访问方式（地址、选轨偏好、请求配置、回调）合为 `Source`，探测与下载共用。

模块：`request`（请求与选项）、`resolve`（拉取播放列表、选轨）、`probe`（探测）、`selection`（选轨的身份）、`ident`（指纹与摘要）、`info`（对外描述来源的类型，不含地址）、`vod`（`plan` 点播计划与摘要，纯计算）、`live`（`session` 续录时的会话判定、`window` 每轨的窗口与新分片、`track` 每轨的刷新与停滞、`merge` 合并输入与缺失报告）、`fetch`（分片下载队列、拉取 init 段）、`workdir`（`record` 任务记录与 job.json 格式、`names` 文件名、`outputs` 正在写的输出的记录）、`output`（`options` 输出配置与路径校验、`hls` 本地 HLS、`commit` 换上输出）、`job`（分派与收尾）、`http`、`crypto`、`verify`、`hooks`（回调）、`report`（进度与结果）、`error`、`blocking`（阻塞线程池）。

### 5.2 任务目录与续传

```
<输出主名>.hsdl/
├── job.json                                                 见下
├── lock                                                     运行期间的排他锁
├── outputs.json                                             正在写的输出，见 5.1
└── tracks/<轨道>/
    ├── <会话>-<起点>-<序号>-<不连续段>-<init>-<时长>-<身份>.seg   已解密且通过校验的分片
    └── init-<指纹>.mp4                                            init 段，按内容命名
```

- 分片文件名：会话见 5.9（点播恒为 0），起点为这条轨在这个会话从哪里开始录（`fresh` 为从窗口起点，`after<序号>` 为跳过不超过该序号的分片，见 5.9；点播恒为 `fresh`），不连续段为会话内的编号，init 为所用 init 段的内容指纹或 `none`，时长为 EXTINF 声明的微秒数，身份为分片身份的指纹。指纹为 SHA-256 的前 8 字节。合并与续录要用的都在分片自己的文件名里，只凭目录内容即可合并或续录。
- **分片身份** = 地址的最后一段（不含查询串）加字节范围。CDN 常在主机、路径前段与查询串里放每次会话甚至每次刷新都不同的令牌，最后一段才稳定。点播的计划摘要与直播的比对用同一口径。
- `job.json` 与 `outputs.json` 都带 `format_version`（当前为 7），它是整个任务目录格式的版本：两个文件的字段，分片与 init 段的文件名，以及其中指纹与摘要的编码，任何一项改变都要升；不认识的版本、未知字段一律拒绝。`outputs.json` 记是否已换好（`swapped`）与各输出的种类（`mp4`、`hls`）、输出的绝对路径、任务目录指纹；两个保留名由输出路径与指纹算出，记录只能指向输出旁的保留名。共有字段：`source_digest`（来源摘要）、`selection`（所选变体的带宽、分辨率、编码、音频组、在属性完全相同的变体里排第几，与音频 rendition 的组、语言、名称、在属性完全相同的音频里排第几；来源是媒体播放列表时为 null）。点播另有 `"kind": "vod"` 与 `plan_digest`（计划摘要），直播另有 `"kind": "live"` 与 `url_digest`（完整来源地址的摘要）。各轨的取流方式由 `selection` 决定，不另记。
- 请求配置（请求头、Cookie 等）不写入任务目录，续传时由调用方再次提供同样的请求。
- **完成的判定只看最终文件名是否存在**：先写 `.part`、fsync 后改名，所以存在即完整（断电也成立），不需要逐片记账。
- **来源摘要** = 来源地址（去掉用户名、密码、查询串与片段）与选轨偏好（音频语言不区分大小写）的 SHA-256。去掉的部分常带每次会话不同的凭据、签名与令牌。
- **选轨**：目录里有同一来源的记录、且已有完成的分片时，按记录的变体与音频属性在主播放列表中找回同一条轨，不按偏好重新选（主播放列表增删变体、地址换令牌都不影响；属性完全相同的冗余变体按排位区分）；找不到报 `WorkDir(SelectionGone)`。
- **计划摘要**（点播）= 各轨各分片的序号、时长、不连续段序号、分片身份与 init 段身份的 SHA-256；不含 key URL 与 IV，因为目录里存的是解密后的分片。摘要不同报 `WorkDir(PlanChanged)`。init 段每次重新拉取（地址常带每次会话不同的签名），按完整地址去重，按内容命名，同组内容不同报 `Unsupported(InitChangesWithinGroup)`。
- 记录与当前任务不一致时报错：来源不同 `SourceMismatch`，点播与直播不同 `KindMismatch`，轨道不同 `TracksMismatch`，计划不同 `PlanChanged`；直播的完整地址不同见 5.9。目录里还没有已完成的分片时（只有 init 段也算没有，它们随时可以重新拉取），直接改为当前任务。没有 `job.json` 的非空目录（`.part` 残留与锁文件除外）不当作任务目录，因为成功后整个目录会被删除。
- 运行期间持有 `lock` 的排他锁，第二个任务打开同一目录时报 `WorkDir(Locked)`。删除任务目录时只删本库写的文件（`job.json`、`outputs.json`、分片、init 段与它们的 `.part`）、系统自动生成的元数据文件（`.DS_Store`、`Thumbs.db`、`desktop.ini`，以及 macOS 在 exFAT 等不能存扩展属性的卷上为每个文件与目录生成的 AppleDouble 文件 `._<名字>`，按名字与开头的魔数认，随主文件留下或删除）与因此变空的目录，其他文件（例如经符号链接或只差大小写的路径写进来的输出）留下，记为删除失败，`job.json` 也留下，目录仍是任务目录、下次照常使用；判断目录是否为空时同样不算系统元数据文件；持锁删完后删锁文件，再释放锁、删除目录：释放锁后别的任务即可打开它，看到的不会是删到一半的任务。
- 输出已存在：MP4 拒绝覆盖，除非调用方明确要求；HLS 目录须不存在或为空，要求覆盖时只在其中全是本库写出的文件时替换（系统自动生成的元数据文件不算）；路径上是别的东西（MP4 路径是目录，HLS 路径不是目录或里面有别的文件），或建不出来（最近的已存在的上级是文件或悬空的符号链接，报错带上它）时报 `OutputOccupied`，覆盖也不替换，以免路径给错时删掉别人的文件。任务开头（在阻塞线程池中，结果经 `Job::wait` 返回，`Engine::start` 只做不访问文件系统的参数校验）与写出前、换上前各检查一次。检查时连根都不存在（Windows 上不存在的盘符、连不上的共享）即报出；输出所在目录在任务开头、检查通过后创建，没有权限等在下载之前报出，代价是任务失败或取消时可能留下空目录；写出前再建一次，下载期间被删就重建。相对路径在开始任务时按当前目录补全。输出与任务目录的路径不能相同或互相包含（按字面比较）：成功后要删除任务目录。
- 放弃任务用 `Engine::discard(work_dir)`：加锁，按 `outputs.json` 收拾写到一半的输出（同 5.1 的规则，原处是别的东西而没放回的旧输出作为残留返回），再像成功后那样只删本库写的文件，返回留下的东西；记录是另一个格式版本写的报 `WorkDir(UnsupportedVersion)`（带文件名、记录的版本与支持的版本）、无法解析报 `WorkDir(Corrupt)`，都什么也不动；目录正被使用报 `WorkDir(Locked)`，不是本库建立的任务目录报 `WorkDir(NotEmpty)`。调用方不要自己整个删除任务目录：里面可能有不是本库写的文件，也会丢掉没能放回的旧输出的记录。
- 任务目录位置可配置：库的默认位置与输出同级，名为输出的主名（MP4 去掉扩展名，只输出 HLS 时为目录名）加 `.hsdl`（`OutputOptions::resolved_work_dir`），`a.mp4` 与 HLS 目录 `a` 共用，合并 MP4 失败后改为输出 HLS 不必重下；成功后删除，没删干净记在结果的 `leftovers` 里；桌面应用放在应用数据目录，不放在下载目录，避免被网盘同步。
- 错误信息中不输出请求头的值、Cookie、key 内容，以及地址中的用户名、密码与查询串。

### 5.3 调度、超时、重试、取消

- `Engine` 持有全局上限：所有任务合计的在途 key、分片与点播 init 段请求数；每次尝试从发出请求到读完响应体占用一个名额，退避等待期间不占。播放列表与直播刷新时新出现的 init 段不占名额，以免直播刷新排在大批下载之后。每个任务另有自己的并发数（同时下载的分片数）。
- 每个请求都有连接超时、读空闲超时和单次尝试的总时长上限，都不能为 0。
- 可重试：连接与传输错误、超时、5xx、408、429。退避为指数加随机抖动，次数可配置；服务器要求的等待（Retry-After）超过 `max_delay` 时不再重试，错误里带上这个等待，直播刷新据此推迟下次刷新。
- 不可重试：其余 4xx、请求无法构造（含回调给出的不合法地址或请求头）、解密或校验失败、回调报错。
- 点播任一分片最终失败：取消该任务其余请求，任务以 `Segment` 结束；已完成的分片保留，可以续传。直播的失败处理见 5.9。
- 取消：每个任务一个 `CancellationToken`，按调用逐层传入每个请求与回调调用；下载队列持有其子令牌，只用来取消自己派出的下载。任务内的下载与刷新由 `JoinSet` 持有，结束前全部等回；回调与文件写入在阻塞线程池中执行，取消时等它们返回。`Engine::start` 返回的 `Job` 只用来等结果，取消、停止与读进度经可克隆的 `JobControl`，等结果期间也能调用；丢弃 `Job` 即取消；写出阶段（合并 MP4、复制 HLS）不响应取消，已开始写出的任务会在后台完成。
- 运行时：`core` 不自建 tokio 运行时，使用调用方的（Tauri、`pyo3-async-runtimes` 都基于 tokio）。

### 5.4 HTTP

- `reqwest` 0.13.5（rustls）。请求配置包括请求头（Cookie、User-Agent 都经请求头传入）与代理；Cookie 在任务内由响应自动保存。
- 证书校验默认开启；关闭必须显式写 `insecure: true`。证书校验失败报 `HttpError::Certificate`、跟随重定向失败（循环、次数过多）报 `HttpError::Redirect`，都不重试；rustls 的证书错误包在 `io::Error` 里，沿原因链逐层取出它包着的错误才认得出，故直接依赖 rustls（与 reqwest 用的同一个 0.23.45）。连接与传输错误的说明带上各层原因。
- 字节范围分片用 `Range` 请求头；响应必须是 206 且长度一致，否则视为错误。
- key 按 key URL 去重，只取一次，缓存在任务内存里。

### 5.5 解密与校验

- AES-128-CBC 加 PKCS#7，使用分片自己的 key 与 IV（`aes` 0.9.3 + `cbc` 0.2.1）。
- 调用方已知 key 时可给自定义 key（`KeyOverride`，可另给 IV）：加密的分片一律用它解密，不请求 key 地址、不经 `on_key`。
- 校验（不通过即失败，不写盘）：
  - 响应体长度与 `Content-Length`、字节范围一致；
  - 解密后去填充必须合法；key 错误时这一步几乎必然失败。IV 只影响第一个 16 字节块，IV 错误发现不了，所以自定义 IV 只在确知时给；
  - 没有 init 段的分片：MPEG-TS（偏移 0 处、长度够时偏移 188 处为同步字节 `0x47`），或 RFC 8216 3.4 的打包音频（AAC 的 ADTS、MP3、AC-3、E-AC-3，前面可有 ID3 标签）；
  - fMP4 分片与 init 段：开头必须是合法的 box 头（`styp`、`moof` 等）。

### 5.6 进度

- `watch` 通道发布进度快照：当前阶段；所选的变体与音轨（有已完成分片的任务目录按记录找回，不一定是偏好会选的那条）；已完成分片数、总分片数（直播随录制增长）、直播取不到的与窗口已滑过的分片数（前者计入总数，后者不计）；第 0 条轨已完成分片的时长（直播即已录时长）；已落盘字节数（以上均含续传前已完成的）；本次从网络读到的字节数（每读到一块即计入，含播放列表、key 与失败的请求）；直播各轨最近一次刷新失败的原因（这条轨刷新成功、不再刷新或录制结束即清空）。速度由消费者按读到的字节数的变化计算。消费者慢只会跳过中间值，不会阻塞下载。
- 最终结果通过 `Job::wait` 的 `Result` 返回，不经进度通道传递。

### 5.7 扩展点（回调）

```rust
// 默认实现均为原样返回；HookError = Box<dyn std::error::Error + Send + Sync>，原样保存在 Error::Hook 中
pub trait Hooks: Send + Sync {
    fn on_playlist(&self, url: &Url, body: Vec<u8>) -> Result<Vec<u8>, HookError>;        // 结果须为 UTF-8
    fn on_request(&self, purpose: Purpose, req: &mut RequestParts) -> Result<(), HookError>; // 改地址与请求头，每次尝试调用一次
    fn on_key(&self, key_url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError>;          // 结果须为 16 字节
    fn on_segment(&self, url: &Url, data: Vec<u8>) -> Result<Vec<u8>, HookError>;          // 解密之前；init 段不经过
}
```

- `on_playlist` 收原始字节：加密、压缩或非 UTF-8 编码的播放列表由回调还原。
- `on_key` 用于站点自定义的 key 变换；`on_segment` 用于去掉分片前的伪装字节（例如伪装成 PNG 的 TS）。
- 回调在阻塞线程池中执行；出错时任务失败（`Error::Hook`，带出错的是哪个回调），错误不可重试；取消不打断正在执行的回调。

### 5.8 错误模型

定义以 `crates/core/src/error.rs` 为准。按调用方的处理方式分类：

| 类别 | 变体 | 可否重试 |
|---|---|---|
| 改请求再试 | `InvalidInput`；`OutputExists`（要求覆盖）；`OutputOccupied`（换路径，含上级不是目录而建不出来的）；`Select`（选轨偏好与来源不符）；`Unsupported::Live`（开启直播录制）；`Unsupported::Mp4`、`Unsupported::Hls`（内容放不进这种输出，改用另一种） | 改请求后再试 |
| 来源本身不行 | `Playlist`（内容不是合法的播放列表）、`NotMediaPlaylist`、`Unsupported` 的其余各项（无分片、不支持的加密、无法合并的布局）、`Integrity`、`KeyLength` | 否 |
| 网络 | `Http`（在最外层时来自播放列表或 init 段的请求；证书校验失败、重定向失败不重试） | 看 `HttpError::retryable` |
| 说明出在哪里 | `Segment`、`Key`（哪个分片或 key）；`LiveStalled`（直播哪条轨停滞，取不到时带 `HttpFailure`，刷新失败时带 `RefreshCause`）；`Cleanup`（失败后收拾也没做完，带残留列表） | 看包着的原因 |
| 读写文件、合并 | `Io`；`Remux`（FFmpeg 初始化、读分片、写出或回读核对出错，内容放不进 MP4 的除外） | 否 |
| 任务目录 | `WorkDir`（`WorkDirProblem`，另一个格式版本写的为 `UnsupportedVersion`）、`NothingRecorded` | 否 |
| 其他 | `Hook`（调用方的回调出错）、`Cancelled` | 否 |

可否重试一律看 `Error::retryable`，服务器要求的等待看 `Error::retry_after`，两者都看包着的原因；等待以秒计可能大到 `u64::MAX` 秒，绑定层按秒数暴露，不转成会溢出的时间间隔类型。

- 收尾没删掉或放回的东西（成功时的 `Output::leftovers`、`Cleanup` 里的 `leftovers`）是结构化列表，每项有路径、种类与原因，种类按调用方的处理方式分：可以直接删除的（写了一半的临时输出、被替换的旧输出等本库写的东西）、没能放回原处的旧输出（带原来的路径）、写输出失败后没能撤下的新输出（完整可用）、没删干净的任务目录（里面可能有不是本库写的文件，不要整个删除）。界面据此提供打开位置或删除，不必解析文案。
- 错误信息已包含原因，不经 `source()` 重复给出；地址只显示到路径，不含用户名、密码与查询串，播放列表里无法解析的地址不显示；代理地址不含用户名与密码。
- 不变量被违反（代码本不该产生的状态）用 panic 中止当次任务，不降级。

### 5.9 直播录制

- **判定**：所选的媒体播放列表有任一没有 `#EXT-X-ENDLIST` 即为直播。任务目录是同一来源、已有完成分片的直播录制，且请求开启了直播时，即使播放列表已结束也按直播继续；没开启时按点播运行，报 `WorkDir(KindMismatch)`。`JobRequest::live` 默认录制，为 None 时遇到直播报 `Unsupported::Live`。
- **起点**：当前播放列表中的全部分片（EVENT 即从头，滑动窗口即从窗口起点）。
- **刷新**（RFC 8216 6.3.4）：各轨独立；播放列表有变化（出现新分片或窗口前移）后，从开始加载起至少等一个 target duration，没变化时等半个，间隔不短于 100 毫秒；刷新失败时等半个 target duration 与服务器要求的等待中较长者。
- **一致性**：分片的身份（地址最后一段与字节范围）与不连续段编号须与上次刷新一致，否则结束录制（`LiveEnd::Inconsistent`）；媒体序号回退且与上次窗口不重叠，连续两次即结束录制（`LiveEnd::Restarted`，多为编码器重启；一次多半是 CDN 的旧缓存）。没有分片的播放列表不参与比对。
- **不连续段编号**：按 RFC 8216 是绝对的（没写 `EXT-X-DISCONTINUITY-SEQUENCE` 即为 0）。服务器不守规定、带 DISCONTINUITY 的分片滑出后编号整体变小时，靠与上次重叠的分片校正；没有重叠时沿用当前校正，算出的编号比已用过的小则新分片另起一组，不打乱分组顺序。
- **缺失**：两次刷新之间已滑出窗口的分片，以及列出了但 404/410 或重试后仍失败的分片与 init 段，记为缺失，录制继续；进度里计数，结果里给出序号区间与原因。缺失不插入不连续标记：保留原时间戳，成片在该处时间线留空，各轨之间的同步不受影响（RFC 8216 不允许用媒体序号在轨间同步，人为切组需要这样做）。FFmpeg 读 fMP4 默认按 `tfdt` 取时间戳，TS 自带时间戳，空档都能保留。
- **失败**：刷新取不到（含 404/410）、内容为空或语法错误（服务器未写完）时等下次刷新；其他刷新失败（401/403 等、内容不是播放列表、DRM、回调出错）以及分片的 key 失败、校验失败、回调出错、写盘失败，使任务失败。
- **init 段**：要录的分片引用的 init 段首次出现时拉取（不占引擎名额；续录时会话定下后才拉），按内容命名，签名地址每次刷新都不同也只存一份；同一不连续段内 init 内容变化时任务失败。init 段先落盘，再下载引用它的分片。
- **停滞**：任一轨持续 `stall_timeout`（默认 60 秒，至少三个目标时长）没有录到新分片，从录制开始起算，会话定下时重新起算；有分片排着队或在下载、或续录判定期间已拿到候选（见「会话与续录」，在等核对或等其他轨）时是在等本任务，不算。播放列表不再出新分片（两个目标时长内没有列出过；`StallCause::NoNewSegments`）或已被删除（`PlaylistGone`），且其他轨也不再出，视为直播结束：判定期间停滞的是还没拿到候选的轨，它们这次都不录，其余照常定下会话；之后把已拉到的处理完、照常合并。以下为故障，任务以 `Error::LiveStalled` 失败，目录保留：刷新一直失败；刷新请求超过一个目标时长仍未返回；仍在列出新分片却一个都下载不成功（带最近一次失败的原因，能否重试随之）；这条轨不再出新分片或播放列表已被删除而其他轨仍在出（`TrackStopped`，只有前者可以重试）。服务器要求的重试等待长到无法表示时不再刷新，由停滞判定结束。
- **结束**：所有轨出现 `#EXT-X-ENDLIST`；调用方 `JobControl::stop`；各轨都录满 `max_duration`（每轨之前各会话已录的、本次排入下载的与本次列出了但取不到的分片声明时长之和；窗口已滑过的时长未知，不计）；停滞；或上述服务器前后矛盾。之后不再刷新，把已拉到的播放列表处理完、已列出的分片下完再合并；续录时会话定下之前停止则立即结束，不录新的分片。
- **会话与续录**：会话是一段时间线连续的录制，合并时组内保留原时间戳、组与组首尾相接。各轨在每个会话的起点（从窗口起点录，或跳过不超过某序号的分片）写在这个会话的每个分片的文件名里。中断后用同样的请求再次运行即续录，先判定会话再录：
  - **候选**：一条轨在判定期间拉到的第一份有分片的播放列表。判定期间各轨照常刷新，拉到的都暂存起来，会话定下后按顺序处理；与上一份暂存的接得上（序号连续、重叠部分身份与不连续段序号一致）的合成一份，暂存的量只随新列出的分片增长、什么也不丢，接不上的分开暂存交给窗口比对。已录满 `max_duration` 的轨、拉到的播放列表已结束而没有分片的轨这次不录、不参与判定。
  - **核对**：候选与该轨最近一次有分片的会话重叠（同一序号、同一身份、编号一致），且重叠的分片中从新到旧第一个取得到的重新下载后与已存的逐字节相同，即为接得上；编码器重启后序号与文件名都可能从头再来，只看文件名分不出新旧。之前一个分片都没录到的轨没有可核对的，不阻挡接续，在定下的会话里从窗口起点录。取不到的重叠分片（404/410，或重试后仍失败的临时故障）换更旧的试；都取不到时，有临时故障则如实上抛，全是 404/410 则按接不上算；其余失败（403 等、key、校验、回调）如实上抛。核对在后台进行，不耽误其他轨刷新。
  - **接续**：要录的各轨都接得上各自最近的会话时，接着目录里最近的会话录：最近的会话就是它的轨补录窗口内没录完的分片（多为中断时在途的），补录不越过该轨在这个会话的起点；最近的会话更早的轨跳过录过的序号并入。否则另起一个会话，接得上的轨跳过录过的序号。一个会话必须各轨一起接上：只接一部分轨，组与组衔接时各轨会错开。
  - **换地址**：来源相同而完整地址与记录的不同（如换了令牌）时，要录的各轨都接得上、且至少一条核对一致，才续录并改记新地址，否则报 `WorkDir(SourceUnverified)`；这次没有要录的轨时不改记，照常合并。
- **合并**：按（会话, 不连续段）分组，只合并各轨都有的组，其余记为缺失（`Unmergeable`）；同一会话相邻已完成分片之间缺的序号，本次运行记下原因的照原因报告，其余记为原因不明（`Unknown`）；缺失不落盘，之前的运行里某个会话第一个已完成分片之前或最后一个之后缺的不在报告里；一个可合并的分片也没有时报 `NothingRecorded`。

## 6. remux

```rust
pub struct TrackSegments { pub init: Option<PathBuf>, pub segments: Vec<PathBuf> }   // 一条轨在一组内的分片
pub struct DiscontinuityGroup { pub tracks: Vec<TrackSegments> }                     // 各组轨道顺序一致
pub enum Streams { All, Video, Audio }                                               // 一条轨贡献的流种类，各组相同
pub fn remux(streams: &[Streams], groups: &[DiscontinuityGroup], output: &Path) -> Result<Report, Error>;
// Report：每路输出流的编码参数（编码、宽高或采样率与声道）、包数与呈现时长
```

- 组内：分片按字节顺序当作一个连续的输入读取（fMP4 时 init 段在前），经 ffmpeg-next 的自定义 IO 交给 FFmpeg；同一时刻只打开一个分片文件，不先拼成大文件。FFmpeg 自带的 concat 协议会同时打开全部文件，一组几百个分片就超过进程的文件描述符上限（macOS 图形程序默认 256）。
- 组间：整组共用一个时间偏移，保留组内各轨（视频与独立音频 rendition）原有的相对时序。偏移取两个下限中较大者：本组最早的 PTS 不早于此前所有流的最晚结束时刻；每路流本组首个 DTS 严格大于上一组末个 DTS（加一个输出时间基 tick 的余量吸收舍入）。按呈现而非 DTS 对齐，B 帧的解码提前量不会在组边界留下空隙。
- 每条轨按 `Streams` 贡献第一路视频和（或）第一路音频，没有这类流的轨不贡献（如只有音频的变体又选了独立音频：输出只有音频），各轨都没有时报 `remux::Unsupported::NoStreams`；未指定种类的流不进输出、不检查编码；同类流只能来自一条轨；后续组的流种类与编码参数（编码、宽高、采样率、声道数）必须与第 0 组一致，否则报 `remux::Unsupported::ParamsChanged`（流种类不同为 `LayoutChanged`），由 core 决定如何处理（例如分辨率不同的广告段）。
- 每路流的解码时间戳在输出时间基下必须严格递增（组内往回跳多为来源漏标了 `EXT-X-DISCONTINUITY`，重复多为相邻分片首尾重复了一帧），否则报 `remux::Unsupported::DtsNotIncreasing`；FFmpeg 的封装器对此只报「参数不合法」（上一个为 0 时还不查），这里在写包前按严格递增一律查出来，与写盘失败分开。
- 只接受 H.264、HEVC 与 AAC，其余编码报 `remux::Unsupported::Codec`。内容放不进 MP4 的各种都归在 `remux::Error::Unsupported` 一个分支，core 转为 `Unsupported::Mp4`（载荷在 core 里叫 `Mp4Unsupported`，与本地 HLS 的 `HlsUnsupported` 对称），与写盘等其他合并失败（`Error::Remux`）分开：调用方据此建议改为输出本地 HLS。
- 写到调用方给的路径（core 给临时名，成功后改名），写完回读核对包数并落盘，失败时删除；其余约束见 ADR-0002：`Packet::read`、HEVC 标 `hvc1`、moov 前置。

## 7. Python 绑定

```python
# 草案
import hs_m3u8

async def main():
    dl = hs_m3u8.Downloader(max_concurrency=32)
    job = dl.start(
        "https://example.com/master.m3u8",
        output="downloads/日日是好日.mp4",
        headers={"Referer": "https://example.com/"},
        on_key=decrypt_key,          # (key_url: str, data: bytes) -> bytes
        on_request=None,             # (req: hs_m3u8.Request) -> None，可改 url 与 headers
        on_playlist=None,            # (url: str, text: str) -> str
        on_segment=None,             # (url: str, data: bytes) -> bytes
        hls_dir=None,                # 另给目录时同时输出可直接播放的本地 HLS；只要 HLS 时 output=None
    )
    async for p in job.progress():   # 定时采样的进度快照
        print(p.segments_done, p.segments_total, p.received)
    result = await job               # 失败抛 hs_m3u8.DownloadError 的子类

hs_m3u8.download("https://...", output="a.mp4")   # 同步版本
```

- PyO3 0.29.3 + `pyo3-async-runtimes` 0.29.0（tokio）+ maturin 1.15.0；`abi3-py311` wheel（最低 Python 3.11），FFmpeg 静态链接进扩展模块。
- Python 回调在阻塞线程池中获取 GIL 后执行，不占用 tokio 工作线程。
- `core::Error` 在边界上翻译为 Python 异常层级，`retryable` 等字段作为异常属性保留。
- 0.1.x 参数对应关系：`key` → `key=hs_m3u8.Key(...)`；`get_m3u8_func` → `on_playlist`；`*_request_before` → `on_request`；`key_response_after` → `on_key`；`ts_response_after` → `on_segment`；`merge`、`del_hls` → 输出目标：MP4、可播放的本地 HLS 目录，或两者都要。

## 8. 桌面应用（GUI 阶段细化）

- Rust 侧持有 `Engine` 和任务存储（SQLite，rusqlite 0.40.2）。命令：新建、暂停、继续、删除（`Engine::discard`）、列表；事件：进度，每个任务限频推送。
- 任务存储保存续传所需的请求配置（请求头、Cookie 等），明文保存，数据库文件仅本人可读写，不加密。
- 事务边界在应用的领域层；存储层接收已开启的连接或事务，不自己开事务。
- 前端技术栈见 ADR-0005；界面布局与主题（倾向明亮下载器风）在 GUI 阶段设计。
- 以后接入外部下载器（ADR-0004）时，多协议任务管理在这一层用任务类型区分，核心库不变。
- 资源嗅探：内嵌浏览器 + 注入脚本 + `cookies_for_url`。远程页面调用的上报命令只接收候选地址，内容按不可信数据校验。需先做原型。

## 9. 并发与生命周期约定

- 每个并发单元都有归属者（任务的 `JoinSet`）、退出条件（完成、失败或取消）和超时。直播录制的刷新、拉 init 段与核对内容是后台任务，由录制的事件循环持有，循环不等单个网络请求；录制结束时协作取消并等它们退出，在途的回调返回后才结束。
- 取消信号一路传到每个请求与回调调用。
- 不用 sleep 做同步；退避等待使用可被取消的定时器。

## 10. 测试与质量门

测试只写两类（项目约定）：

- 纯计算：`hls` 的规范化规则（KEY/MAP/BYTERANGE/不连续传播、IV 推导、URL 解析、选轨）、计划摘要、AES 解密（已知向量）、分片校验。
- 编解码与端到端：
  - `remux` 用仓库内的小样本（ffmpeg testsrc 生成，几百 KB）覆盖 TS、fMP4、音视频分离、HEVC、不连续，断言每条流的包数和帧数；
  - `core` 的端到端在测试内起本地 HTTP 服务提供 HLS 样本（含 AES-128、字节范围、重定向、按请求次数变化的直播播放列表，注入 404、500 与阻塞），跑完整任务后把输出与直接合并同一批样本的结果逐字节比较，并覆盖取消、续传与续录；
  - Python 绑定做一个端到端冒烟（Python 绑定阶段）。

不写用假件拼流程、复述实现的测试。

检查命令（`make check`，零告警才算通过；Rust 部分为 Makefile 的 `rs_*` 目标，CI 调用同样的目标）：

1. `cargo fmt --check`
2. 依赖方向：`scripts/check_deps.sh` 用 `cargo tree` 断言第 3 节（含传递依赖与全部目标平台）
3. 禁止压制属性：`scripts/check_no_suppression.sh`（`#[allow]`、`#[expect]`，含 `cfg_attr` 包裹的）
4. `cargo deny check`（`deny.toml`）：安全公告与撤回版本、许可证白名单（只允许宽松许可证；FFmpeg 的 LGPL 由其构建脚本检查）、禁用 OpenSSL、依赖只来自 crates.io
5. `cargo clippy --workspace --all-targets -- -D warnings`
6. 文档：`cargo doc --workspace --no-deps --document-private-items`，告警即失败（文档注释里的链接须都能解析）
7. `cargo test --workspace`
8. 覆盖率：`hls`、`core`、`remux` 各自行覆盖 ≥ 80%（cargo-llvm-cov）
9. Python：maturin 构建 + 端到端冒烟（Python 绑定阶段加入）
10. 前端：`tsc --noEmit`、lint、构建（GUI 阶段加入）

## 11. CI 与发布

- GitHub Actions 三平台矩阵：macOS arm64、Windows x64、Linux x64；FFmpeg 静态库单独构建并缓存。
- PyPI：每个平台一个 abi3 wheel。
- 桌面：tauri-action 出安装包；自动更新用 tauri-plugin-updater，更新签名密钥自行生成并存于 CI 密钥。macOS 签名与公证在购买开发者账号后接入；Windows 不签名。

## 12. 待验证风险与待定问题

按顺序验证：

1. 高并发下 Python 回调的 GIL 竞争（`on_segment` 每个分片调用一次）。
2. 资源嗅探的注入脚本（GUI 阶段）。

待定：

1. 桌面应用的产品名（发布前定；PyPI 包名仍为 `hs-m3u8`）。

## 依据的版本（2026-10-08 查 crates.io）

tokio 1.53.2、tokio-util 0.7.19、rustls 0.23.45 与 tokio-rustls 0.26.6（2026-10-10，取 Cargo.lock 中 reqwest 已用的版本；后者仅测试用）、reqwest 0.13.5、axum 0.8.9（2026-10-09 查，仅测试用）、aes 0.9.3、cbc 0.2.1、url 2.5.8、thiserror 2.0.21、serde 1.0.229、serde_json 1.0.151、sha2 0.11.0、rusqlite 0.40.2、pyo3 0.29.3、pyo3-async-runtimes 0.29.0、maturin 1.15.0、ffmpeg-next 9.0.0、tauri 2.12.1、cargo-deny 0.20.2、cargo-llvm-cov 0.9.1（2026-10-09 查）、libc 0.2.190（2026-10-10 查，仅测试用）。
