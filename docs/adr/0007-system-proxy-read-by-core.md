# ADR-0007：系统代理由本库读取并逐项检查

- 状态：已采纳
- 日期：2026-10-11

## 决定

「按系统设置」的代理不交给 reqwest 自己读，由 `core` 的 `proxy` 模块读取、逐项检查，用不了的值报参数错误，结果显式交给 reqwest（`Proxy::custom`）。

- 先看环境变量，规则同 curl：http 地址看 `http_proxy`，https 地址看 `https_proxy`、`HTTPS_PROXY`，都没设时看 `all_proxy`、`ALL_PROXY`；小写的优先；空值等于没设（curl 的 `getenv.c` 把空值当作没设）。另认大写的 `HTTP_PROXY`（curl 不认，Python、reqwest 都认），只在设了 `REQUEST_METHOD`（CGI）时不认：只按 curl 不认它，设了它的用户会悄悄直连。`no_proxy`、`NO_PROXY` 为例外，对环境变量与操作系统设置的代理都起作用（与 hyper-util 原先的做法相同）。
- 环境变量里没设代理时看操作系统的设置：macOS 的系统设置（HTTP、HTTPS、SOCKS 代理与例外、「不包括简单主机名」），Windows 的 Internet 选项（`ProxyServer` 含分协议的写法，`ProxyOverride` 含 `<local>` 与通配符）；没设 http、https 代理时用 SOCKS 代理代替（macOS 的为 SOCKS5，Windows 的 `socks=` 为 SOCKS4）。
- 用了自动代理配置（PAC）或 macOS 的自动发现时报错：本库不执行 PAC，也不做发现。Windows 的「自动检测设置」（WPAD）不看：它默认开着，多数网络里并没有可发现的代理，报错会让多数 Windows 用户默认就用不了。
- 读系统设置原文的代码放在单独的 crate `sysproxy`，只读不解析：macOS 例外列表是 CF 数组，用 core-foundation 读出其中的字符串要 `unsafe`（把数组元素当 CF 对象包装），其余 crate 禁止 `unsafe`。这是继 `remux`（ADR-0002）之后的第二个 FFI 边界。解析与检查在 `core`，按纯计算测试。

## 原因（2026-10-11 查证）

reqwest 0.13.5 默认的 `system-proxy` 经 hyper-util 0.1.21 的 `Matcher::from_system` 读取：

- 环境变量的值解析不出（协议写错、`http::Uri` 解析不出主机）时当作没设，请求直连；macOS 上按协议逐个补：环境变量没设的那一项才看系统设置。
- macOS 只读 HTTP、HTTPS 两项，不读 SOCKS、PAC、例外列表（被排除的内网地址反而发给代理）。
- Windows 把 `ProxyServer` 整个当成一个地址，分协议的写法解析不出，也不读 `AutoConfigURL`。

用户以为走了代理而实际直连，暴露真实 IP；这违背核心默认从严的约定。

查证的依据：curl master 的 `lib/proxy.c`（变量名与查找顺序、`no_proxy` 的匹配）与 CURLOPT_PROXY 文档（无协议时为 http、不支持的协议报错）；CPython main 的 `Lib/urllib/request.py`（`getproxies_registry` 的 SOCKS4 默认、`<local>`，macOS 例外的 `169.254/16` 写法）；Chromium 的 `proxy_config_service_mac.cc`（macOS 的 SOCKS 为 SOCKS5）。依赖：system-configuration 0.7.0、windows-registry 0.6.1（原先已经经 hyper-util 在依赖树里）。

## 后果

- 系统代理有问题时，探测与开始任务立即报 `InvalidInput`，指出是哪个来源的哪一项，用户改为直连或指定代理地址。
- macOS 与 Windows 读系统设置的那一层没有端到端测试：改系统设置会影响开发机，只测解析与匹配（纯计算），环境变量另有子进程里的端到端测试。
