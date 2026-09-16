# ox-sse-proxy

`ox-sse-proxy` 是一个本地 Codex HTTP/SSE 代理：监听 `127.0.0.1:18899`，把
Responses API 请求转发到 OpenCode Go 网关；配置的翻译模型会从 Responses 翻译为
Chat Completions，再组装为 Codex 可消费的 Responses SSE。当前运行实现是 Rust/Axum，
旧 Python 文件只作为行为参考。

## 用途与拓扑

```text
Codex ──HTTP 127.0.0.1:18899──▶ Rust/Axum ox-sse-proxy
                                      ├── Go: /zen/go/v1
                                      └── anonymous free: /zen/v1 + Bearer public
```

代理负责协议翻译、SSE 增量事件组装、输入工具调用清洗/重排、免费模型发现与缓存、
有界请求/响应/SSE 事件、瞬态重试、401/403 免费回退和安全日志。默认翻译模型为
`ox-alpha-free,qwen3.7-plus,hy3`；动态 `*-free` 模型和 `big-pickle` 走免费通道。

## 核心能力

- 将 Codex Responses 请求翻译为上游 Chat Completions，并恢复 Responses SSE 事件链。
- 清洗孤儿 function call、修复工具调用顺序，并对透传 SSE 补齐必要的 message 事件。
- 发现、缓存和同步动态免费模型；对 Go 通道 401/403 提供受控免费回退。
- 限制请求体、普通响应和单个 SSE 事件大小；对瞬态上游状态执行有界重试。
- 只记录脱敏 host/path，提供可备份、检查、恢复的 launchd 发布脚本。

## 快速开始

需要 Rust toolchain、Cargo、Python 3 和 macOS 的 `launchctl`/`lsof`。在仓库根目录：

```bash
cargo build --release
./target/release/ox-sse-proxy 18899 https://opencode.ai/zen/go/v1
```

上游访问需要在安全环境中设置 `OPENCODE_GO_API_KEY`。服务启动后，另开终端运行真实
网关冒烟（脚本不会打印 key）：

```bash
export OPENCODE_GO_API_KEY='(secret placeholder)'
python3 scripts/smoke_real_proxy.py --host 127.0.0.1 --port 18899 --model ox-alpha-free
```

成功输出包含 `SMOKE_OK`、HTTP status、首个响应 body/SSE 字节的 TTFB、总耗时和事件计数。只想跑本地 mock 或
仓库测试时，不要把 token 写入源码、plist、日志或命令示例。

## Codex 配置示例

```toml
model = "ox-alpha-free"
model_provider = "opencode"

[model_providers.opencode]
base_url = "http://127.0.0.1:18899"
wire_api = "responses"
experimental_bearer_token = "sk-your-token-placeholder"
```

示例中的 token 是占位符；代理不持久化入站 token，只在 Go 主通道按请求转发，免费
通道强制使用 `Bearer public`。

## 完整环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `OX_PROXY_LABEL` | `com.ox-sse-proxy` | launchd label 与 plist 文件名；安装时渲染进 plist 和 control 脚本 |
| `OX_PROXY_PORT` | `18899` | launchd wrapper/control 使用的本地监听端口 |
| `OX_PROXY_UPSTREAM_BASE` | `https://opencode.ai/zen/go/v1` | launchd wrapper 传给 Rust binary 的 Go base URL |
| `OPENCODE_GO_API_KEY` | 无 | 真实网关入站 Bearer key；不写入文件/日志 |
| `OX_TRANSLATE_MODELS` | `ox-alpha-free,qwen3.7-plus,hy3` | 静态协议翻译模型集合 |
| `OX_PROXY_RETRIES` | `5` | Go 主通道瞬态状态重试次数 |
| `OX_PROXY_TIMEOUT` | `300` | 上游读取/不活跃超时秒数 |
| `OX_PROXY_MAX_TOKENS` | `16384` | 翻译请求缺省 `max_tokens` |
| `OX_PROXY_FREE_FALLBACK` | `1` | Go 401/403 是否回退匿名免费通道；`0` 关闭 |
| `OX_PROXY_FREE_BASE` | `https://opencode.ai/zen/v1` | 免费通道 base URL |
| `OX_PROXY_FREE_CACHE` | `~/.codex/ox_proxy_free_models.json` | 免费模型缓存文件 |
| `OX_PROXY_FREE_REFRESH_SECS` | `900` | 免费集刷新间隔 |
| `OX_PROXY_SYNC_MODELS` | `1` | 是否同步 `models.json`；`0` 关闭 |
| `OX_PROXY_MODELS_JSON` | `~/.codex/models.json` | `models.json` 覆盖路径 |
| `OX_PROXY_MAX_REQUEST_BYTES` | `8388608` | 请求体上限 |
| `OX_PROXY_MAX_RESPONSE_BYTES` | `16777216` | 普通上游响应上限 |
| `OX_PROXY_MAX_SSE_EVENT_BYTES` | `1048576` | 单 SSE 事件上限 |
| `OX_PROXY_LOG_LINES` | `100` | control `logs` 命令 tail 行数 |

Rust binary 本身的命令行是 `ox-sse-proxy [port] [upstream_base]`；其中端口和上游
参数优先于 wrapper 的默认值。路径变量支持 `~/` 展开。

## 测试与构建

```bash
cargo build
cargo build --release
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo audit  # 需要本机已安装 cargo-audit
```

发布资产的静态检查：

```bash
bash -n scripts/*.sh
python3 -m py_compile scripts/smoke_real_proxy.py
tmp_plist="$(mktemp)"
sed -e "s|__HOME__|$HOME|g" \
  -e "s|__LABEL__|${OX_PROXY_LABEL:-com.ox-sse-proxy}|g" \
  scripts/com.ox-sse-proxy.plist.in > "$tmp_plist"
plutil -lint "$tmp_plist"
rm -f "$tmp_plist"
```

## 真实网关冒烟

脚本使用标准库向 `/responses` 发起真实请求，断言 HTTP 200、`text/event-stream`、
`response.created`、`response.completed` 和非空 `response.output_text.delta`。失败时
非零退出，且不会输出响应 body 或 key：

```bash
python3 scripts/smoke_real_proxy.py --host 127.0.0.1 --port 18899 --model ox-alpha-free --timeout 60
python3 scripts/smoke_real_proxy.py --host 127.0.0.1 --port 18999 --model ox-alpha-free --timeout 60
```

本轮已验证事实：已安装并由 launchd 管理的 Rust release 服务 `127.0.0.1:18899`，以及
安装前后用于对照的手工 Rust release 服务 `127.0.0.1:18999`，均真实返回 `SMOKE_OK`。
安装后的最终重启冒烟 TTFB 为 `71957.7ms`；安装阶段冒烟 TTFB 为 `81728.2ms`。两次
耗时都包含真实 OpenCode 上游等待，属于上游波动，不能归因于代理本身。

## launchd 安装、恢复与运维

安装脚本只写安装所需的四个精确文件：`~/.codex/ox-sse-proxy`、
`~/.codex/ox_sse_proxy.sh`、`~/.codex/ox_sse_proxy_launchd.sh` 和
`~/Library/LaunchAgents/<label>.plist`（`<label>` 默认为 `com.ox-sse-proxy`，可用
`OX_PROXY_LABEL` 覆盖）。安装前会构建 release、按时间
创建备份、停止旧 job、原子替换文件、启动并执行严格 check；失败信息会给出备份路径
与恢复命令。

```bash
bash scripts/install_launchd.sh install
bash scripts/install_launchd.sh check
~/.codex/ox_sse_proxy.sh start
~/.codex/ox_sse_proxy.sh stop
~/.codex/ox_sse_proxy.sh restart
~/.codex/ox_sse_proxy.sh status
~/.codex/ox_sse_proxy.sh check
~/.codex/ox_sse_proxy.sh logs
```

恢复最新备份，或恢复指定的备份根直接子目录：

```bash
bash scripts/install_launchd.sh restore latest
bash scripts/install_launchd.sh restore "$HOME/.codex/ox-sse-proxy-backups/YYYYmmddHHMMSS"
```

恢复会停止 job，依据 manifest 恢复原文件；原来不存在的安装产物会被删除；只有原
plist 存在时才重新加载它，并打印恢复后的 status。`latest` 不接受任意路径。备份位于
`$HOME/.codex/ox-sse-proxy-backups/<timestamp>`，直接恢复最新一份：

```bash
bash scripts/install_launchd.sh restore latest
```

如果回滚的是更早版本创建的备份，manifest 中的 plist 名可能沿用当时的 label；恢复后
control 脚本与 plist 会回到备份时的状态，需要重新执行 `install` 才会切回当前 label。

control `check` 必须同时看到 job loaded、本地端口 listener，以及 listener 命令含
Rust `ox-sse-proxy` 且不是 Python；任何一项失败都会非零退出。本轮安装后的 check
确认 listener 为 `$HOME/.codex/ox-sse-proxy 18899 https://opencode.ai/zen/go/v1`。
随后 stop 验证 job 和端口都消失，start 生成新 PID，restart 再生成新 PID 且 check 通过。
wrapper 遇到端口已被占用时成功退出，以避免与手工进程抢端口；正常启动会写 pid 后
`exec ~/.codex/ox-sse-proxy`。

### label 与旧版本迁移

早期版本把个人用户名写死在 launchd label 和 plist 文件名里，现在 label 默认为
`com.ox-sse-proxy`，可用 `OX_PROXY_LABEL` 覆盖（只允许字母、数字、点和连字符）。
`install` 安装前会扫描 `~/Library/LaunchAgents/`，自动停用其他仍指向本项目
`ox_sse_proxy_launchd.sh` 的已加载 job，因此升级无需手工 `launchctl` 操作；旧 plist
文件会保留，回滚备份时仍可使用。

## 故障排查

- `check failed: launchd job ... is not loaded`：先查看 `launchctl print gui/$(id -u)/${OX_PROXY_LABEL:-com.ox-sse-proxy}`，再检查 plist 路径和 `logs`。
- 没有 listener：确认 `OX_PROXY_PORT`、端口占用和日志；wrapper 的端口冲突退出是有意设计。
- listener 若显示 Python：说明目标机不是本文记录的当前部署状态；执行 `bash scripts/install_launchd.sh install`，再以安装后的 `check` 判断。
- 401/403：检查 `OPENCODE_GO_API_KEY` 是否只在当前安全环境提供；主通道失败时免费回退最多两次。
- `response.completed` 缺失：上游 finish reason 为截断、网络错误或 EOF 时 Rust 会保留可观察失败，不伪造完成事件。
- 502 transport：查看日志中的脱敏 host/path；代理不会把 query、token、Cookie 或其他凭据写入日志。
- 免费模型不可用：检查 `OX_PROXY_FREE_CACHE`、`OX_PROXY_FREE_BASE` 和 `/models` 返回；旧缓存可在刷新失败时继续使用。
- 真实网关失败：先确认 DNS、网关可达性、key 和模型权限；本地测试/mock 不能替代真实冒烟。

## 安全说明

不要把 `OPENCODE_GO_API_KEY`、Codex token、Cookie、代理凭据或完整真实请求写入仓库、
plist、shell history 或日志。脚本只引用环境变量名，日志路径固定在 `~/.codex`，请求
日志只记录脱敏 host/path。免费通道使用公开 Bearer，不应把入站凭据转发给它。备份目录
可能包含旧 plist/脚本，请按用户目录权限保护；恢复命令只接受备份根直接子目录。

## 当前限制

- 本机 launchd 安装、stop/start/restart/check、端口切换和真实网关冒烟已验证；其他目标机的 DNS、权限和 Codex 端到端行为仍需单独验证。
- `cargo audit` 不会自动安装；真实网关冒烟需要有效的 `OPENCODE_GO_API_KEY`。
- 免费模型发现与 `models.json` 同步依赖上游返回兼容 schema；透传模型不做 Responses→Chat 免费降级。
- 当前 wrapper 只支持单个本地 TCP listener；不同端口/上游需通过 `OX_PROXY_PORT`、
  `OX_PROXY_UPSTREAM_BASE` 在运行环境中显式覆盖，并重新执行 `check`。

## 开源与许可

本项目以 **MIT OR Apache-2.0** 双许可发布，与 Rust 生态惯例一致，你可以任选其一：

- [`LICENSE-MIT`](LICENSE-MIT)
- [`LICENSE-APACHE`](LICENSE-APACHE)

所有直接与传递依赖均为宽松许可（MIT、Apache-2.0、BSD、ISC、Unicode-3.0 等），不含
强制 copyleft 组件，因此可以按上述任一许可自由使用、修改和再分发。除非另有显式声明，
任何以补丁、issue 或 pull request 形式提交的贡献都按同样的双许可条款授权。

开源前的敏感信息复核结论、依赖许可清单和遗留事项见
[`docs/open-source-review.md`](docs/open-source-review.md)。
