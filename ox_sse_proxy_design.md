# ox-sse-proxy Rust/Axum 设计与交接文档

> 写给完全没有上下文的新会话。本文描述当前仓库中的 Rust/Axum 实现；一份私有项目中的旧 Python 实现
> 只是迁移参考基线，不包含在本仓库内，也不是本仓库当前的运行实现。最后更新：2026-08-23。

## 1. 当前状态与边界

本项目是本地 Codex HTTP/SSE 代理，默认监听 `127.0.0.1:18899`，将 Codex 的
`/responses` 请求转发到 OpenCode Go 网关（默认
`https://opencode.ai/zen/go/v1`）。对配置的翻译模型，代理将 Responses 请求翻译为
`chat/completions`，再组装回 Codex 可消费的 Responses SSE；其余 `/responses` 请求走
透传并进行增量 SSE patch。

当前代码入口：

- `src/main.rs`：读取命令行和环境配置，绑定本地地址并调用 `axum::serve`。
- `src/service.rs`：`ProxyConfig`、路由、上游请求/重试、免费模型缓存/同步、请求边界和 SSE 流处理。
- `src/lib.rs`：Responses 输入清洗、工具顺序修复、`translate_request`、`chat_to_sse`、
  `SsePatcher` 和 `ChatStreamAssembler`。

Python 参考基线保留了原始行为决策（输入清洗、协议翻译、免费别名和错误语义），但
不得据此描述当前部署状态或 Rust 的内部实现。

拓扑如下：

```text
Codex ──HTTP 127.0.0.1:18899──▶ Rust/Axum ox-sse-proxy
                                      ├── Go: /zen/go/v1
                                      └── anonymous free: /zen/v1 + Bearer public
```

## 2. 请求分流与协议处理

### 2.1 `/responses`

`POST /responses` 先检查有界请求体并解析 JSON，然后对 `input` 执行：

1. 移除没有对应 `function_call_output` 的孤儿 `function_call`（包括 message content 中的调用）。
2. 按调用批次重排 `function_call`、对应 output 和被夹在中间的纯文本消息。

分流规则：

- `OX_TRANSLATE_MODELS`（默认 `ox-alpha-free,qwen3.7-plus,hy3`）走 `chat/completions` 翻译。
- 动态免费集中的候选模型（`*-free`，但明确排除 `ox-alpha-free`，以及 `big-pickle`）
  直接走 Zen 免费通道，并使用 `Bearer public`。
- 其他模型（例如 `deepseek-v4-flash`）透传 `/responses`，并把请求 model 传给
  `SsePatcher`，使补造事件中的 `response.model` 保持正确。

非 `/responses` 路径（例如 `GET /models`）原样转发，但仍受请求/响应边界和安全日志约束。

### 2.2 Responses → Chat 请求

`translate_request` 的主要映射：

| Responses 字段 | Chat 字段 |
|---|---|
| `instructions` | `role=system` 消息 |
| `input[].message` | `messages[]`；assistant function call 转为 `tool_calls` |
| `function_call_output` | `role=tool` |
| `tools` 中的 `type=function` | Chat function tools；不支持的工具被过滤 |
| `max_output_tokens` | `max_tokens`；缺省 `16384` |
| `reasoning`、`temperature`、`top_p` | 同名字段保留 |
| `tool_choice` | function choice 做对应映射 |

### 2.3 Chat → Responses SSE

`ChatStreamAssembler` 增量输出 `response.created`、`response.in_progress`、输出 item/part、
`output_text.delta`、done、`response.completed` 和 ping。工具调用参数分片先累积，再在
正常 finish 时输出完整调用。非流式 Chat JSON 由 `chat_to_sse` 一次性组装为同一事件链。

正常完成原因是 `stop` 或 `tool_calls`。`length`、network/error 等异常 finish，或上游在
finish/[DONE] 前 EOF，均标记截断且不伪造 `response.completed`；核心层提供的
`ChatStreamAssembler::finish_eof()` 在服务层 EOF 分支调用。这样 Codex 可以观察到失败并自行重试。

透传 `SsePatcher` 是按完整事件块工作的增量状态机：发现缺失 message item 的 delta 时补造
created/in-progress、message item、content part 和 done 事件；如果上游已经先发 message item，
则原样透传。单事件超过上限时流返回可观察的流错误，不继续无界累积。

## 3. 免费模型发现、缓存与同步

免费模型发现请求为 `GET {free_base}/models`，只提取：

- ID 以 `-free` 结尾且不是 `ox-alpha-free`；
- 明确额外免费 ID `big-pickle`。

只有请求 model 是上述免费候选时才触发发现；普通透传和静态翻译请求不会因免费发现而阻塞。
发现网络请求使用 30 秒上限。缓存状态由服务共享的 async `RwLock` 保存，刷新由 async `Mutex`
single-flight 保护：并发请求最多产生一个 `/models` 请求。冷启动从
`OX_PROXY_FREE_CACHE` 读取 `{"free":[...]}`，缓存可立即用于路由；过期刷新失败时保留旧集合。

成功刷新后，缓存写入采用同目录临时文件 + rename 原子替换。磁盘写和 models.json 同步运行在
`tokio::task::spawn_blocking`，不会阻塞 Tokio worker，也不持有 cache `RwLock` 写锁。

当 `OX_PROXY_SYNC_MODELS` 未关闭时，刷新结果会同步到 `OX_PROXY_MODELS_JSON`：

- 只增不删；
- 从已有免费条目优先、否则从 `ox-alpha-free` 克隆模板；
- 原子替换；
- 文件缺失、JSON 无效、`models` 缺失或不是数组、没有兼容模板时只记录诊断日志，当前代理请求仍继续。

免费回退别名顺序为 exact → `<model>-free` → `ox-alpha-free` 的
`x-preview-f-free` 特例。Go 通道 401/403 不进入主重试，随后免费回退最多两次；主通道仍使用
`OX_PROXY_RETRIES`。匿名免费直连和回退只构造必要的安全 headers：`Authorization: Bearer public`、
User-Agent、Accept、Content-Type、Accept-Encoding；不会转发入站 Cookie、X-Api-Key、
Proxy-Authorization 或其他凭据。

## 4. 边界、超时、重试与错误

默认限制：

- 请求体：8 MiB；超限返回 413。Axum `DefaultBodyLimit` 与服务层检查同时覆盖。
- 普通上游响应：16 MiB；超限返回 502。
- 单个 SSE 事件：1 MiB；超限终止流并产生可观察错误。

三个限制均可配置（见 §5）。未知 `Content-Length` 的响应按 chunk 分块累计，超过限制立即停止，
不会先调用无界的 `response.bytes()`。重试体使用共享 `Bytes` clone，不在每次重试深拷贝 Vec。

Reqwest client 使用 15 秒 connect timeout 和 `OX_PROXY_TIMEOUT` 的 read/inactivity timeout；没有
把整个长 SSE 请求限制成一个总时长，因此只要上游持续发送数据，长流不会被错误截断。瞬态状态集为
`404,429,500,502,503,504`；401/403 快速失败。日志只输出经过 `log_target` 处理的 host/path，
不打印 query/token。返回客户端的 transport error 是固定安全消息
`upstream transport failure`，原始错误仅用于服务端诊断且不带完整 URL。

## 5. 配置

### 5.1 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `OX_TRANSLATE_MODELS` | `ox-alpha-free,qwen3.7-plus,hy3` | 静态协议翻译模型集合 |
| `OX_PROXY_RETRIES` | `5` | Go 主通道瞬态状态重试次数 |
| `OX_PROXY_TIMEOUT` | `300` | 上游读取/不活跃超时秒数 |
| `OX_PROXY_MAX_TOKENS` | `16384` | 翻译请求缺省 `max_tokens` |
| `OX_PROXY_FREE_FALLBACK` | `1` | Go 401/403 是否回退匿名免费通道 |
| `OX_PROXY_FREE_BASE` | `https://opencode.ai/zen/v1` | 免费通道 base URL |
| `OX_PROXY_FREE_CACHE` | `~/.codex/ox_proxy_free_models.json` | 免费模型缓存文件 |
| `OX_PROXY_FREE_REFRESH_SECS` | `900` | 免费集刷新间隔；测试可设为短周期 |
| `OX_PROXY_SYNC_MODELS` | `1` | 是否同步 models.json |
| `OX_PROXY_MODELS_JSON` | `~/.codex/models.json` | models.json 覆盖路径 |
| `OX_PROXY_MAX_REQUEST_BYTES` | `8388608` | 请求体上限 |
| `OX_PROXY_MAX_RESPONSE_BYTES` | `16777216` | 普通响应上限 |
| `OX_PROXY_MAX_SSE_EVENT_BYTES` | `1048576` | 单 SSE 事件上限 |

### 5.2 命令行与 Codex 配置

```text
cargo run --release -- [port] [upstream_base]
```

默认端口为 `18899`，默认上游为 `https://opencode.ai/zen/go/v1`。等价的 release binary
启动方式见 §7。

Codex 侧示例：

```toml
model = "ox-alpha-free"
model_provider = "opencode"
[model_providers.opencode]
base_url = "http://127.0.0.1:18899"
wire_api = "responses"
experimental_bearer_token = "sk-..."
```

代理不持久化入站 token，只在 Go 主通道按请求转发；免费通道强制覆盖为 `Bearer public`。

## 6. 当前测试与本地验证

当前仓库共有 29 项测试：5 项配置启动、10 项核心行为、14 项服务集成。服务集成 mock 覆盖：
动态免费直连和 big-pickle、`ox-alpha-free` Go 路由、免费缓存冷启动/刷新失败保留、single-flight、
models.json 同步和坏 schema、匿名 header 清理、patch model、2.3MB 请求、超大响应/SSE event、
重试耗尽、401/403 分流、免费回退两次、SSE EOF 不 completed、transport query secret 不泄露。

仓库内命令：

```bash
cargo build
cargo build --release
cargo test --test config_startup --test proxy_service
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo audit
```

`cargo audit` 依赖本机已安装 `cargo-audit`；它是依赖漏洞检查命令，不由代码仓库自动安装。
集成测试使用临时目录隔离 `OX_PROXY_FREE_CACHE`/`OX_PROXY_MODELS_JSON`，不应写入用户配置。

Sol 验收数据仅限“无上游坏 JSON 本地入口”（3000 请求，c100）：

| 实现 | req/s | 失败 |
|---|---:|---:|
| 当前 Rust/Axum | 15464.48 | 0 |
| 旧 Rust | 14149.61 | 0 |
| Python 参考基线 | 1373.39 | 0 |

上游 GET mock 受 mock server 瓶颈，约 59–61 req/s；该数据不用于宣称端到端吞吐提升。

## 7. 部署状态与安全运行方式

本机已通过 `bash scripts/install_launchd.sh install` 完成 Rust release binary 的 launchd
部署。安装后的 `~/.codex/ox_sse_proxy_launchd.sh` 当前执行：
`$HOME/.codex/ox-sse-proxy 18899 https://opencode.ai/zen/go/v1`，launchd
check 已确认该 listener 是 Rust binary 而不是 Python。安装备份位于
`$HOME/.codex/ox-sse-proxy-backups/<timestamp>`。

安装后曾执行 stop，确认 job 和端口都消失；随后 start 生成新 PID，restart 再生成新 PID
且 check 通过。最终重启后的真实网关冒烟返回 `SMOKE_OK`，事件包含
`response.created`、`response.output_text.delta`、`response.completed` 和 `ping`，TTFB
为 `71957.7ms`；安装阶段冒烟 TTFB 为 `81728.2ms`。这两个 TTFB 包含真实 OpenCode
上游等待，属于上游波动，不能归因于代理本身。旧 Python 文件仍保留在本仓库之外的
私有项目中，仅作迁移参考，不是当前 launchd 运行实现。

仅在仓库内构建/运行 Rust 的方式：

```bash
cargo build --release
./target/release/ox-sse-proxy 18899 https://opencode.ai/zen/go/v1
```

或：

```bash
OX_PROXY_FREE_CACHE=/tmp/ox_proxy_free_models.json \
OX_PROXY_MODELS_JSON=/tmp/ox_models.json \
cargo run --release -- 18899 https://opencode.ai/zen/go/v1
```

上述手工命令只证明仓库内 binary 可运行；当前本机的 launchd、端口和真实网关状态已由安装后
check、启停/restart 和真实冒烟单独验证。其他机器的 DNS、权限和 App/Codex 端到端状态仍需
单独验证，不得用本地 mock 结果替代。

## 8. 排查指南

| 症状 | 排查路径 |
|---|---|
| `response.completed` 缺失 | 查看 `finish_reason`、上游 EOF/读取错误；这是截断保护，不应伪造完成事件 |
| 400 `No tool output found` | 检查 `src/lib.rs` 输入清洗和工具批次重排；先用 mock/测试复现 |
| 免费模型误走 Go | 确认 `/models` 返回 ID；`ox-alpha-free` 永远不是仅凭后缀识别的免费模型 |
| 免费刷新失败 | 确认缓存文件仍可读；旧集合应继续使用；检查 models.json 诊断日志 |
| 401/403 风暴 | 主通道只尝试一次后进入最多两次免费回退；检查安全 header 和免费 alias |
| 502 transport | 客户端只看固定错误；服务日志只看脱敏 host/path，不应出现 query/token |
| 2.3MB 请求被拒绝 | 检查 `OX_PROXY_MAX_REQUEST_BYTES` 和 Axum body limit，确认至少大于请求字节数 |
| Rust 与 launchd 行为不同 | 先执行 `~/.codex/ox_sse_proxy.sh check`；确认 listener 命令为 Rust binary，而不是旧 Python 参考文件 |

## 9. 后续工作

- 如需回滚本轮部署，执行：`bash scripts/install_launchd.sh restore latest`。
- 为免费集轮换建立明确告警和运维记录。
- 若需要透传模型的免费回退，新增明确的 `/responses` → Chat 降级设计与测试；当前回退只作用于翻译路径。
