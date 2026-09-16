# 外部运行资源

本文只记录访问方法和路径，不记录任何 token、账号或实际 secret。

## 真实网关

真实网关冒烟通过本机已运行的 `ox-sse-proxy` 访问。运行冒烟前，在当前 shell
中提供环境变量 `OPENCODE_GO_API_KEY`；代理会把它作为入站 Bearer 凭据转发到
OpenCode Go `/chat/completions`。使用仓库脚本：

```bash
export OPENCODE_GO_API_KEY='(在安全环境中设置，不要写入文件或命令示例)'
python3 scripts/smoke_real_proxy.py --host 127.0.0.1 --port 18899 --model ox-alpha-free
```

脚本只输出状态码、TTFB、总耗时、SSE 事件计数和文本增量长度，不输出 key。

## launchd runtime

- Label：`com.ox-sse-proxy`（可用 `OX_PROXY_LABEL` 覆盖）
- 用户域：`gui/$(id -u)`
- 查看：`launchctl print gui/$(id -u)/${OX_PROXY_LABEL:-com.ox-sse-proxy}`
- 安装后的控制脚本：`~/.codex/ox_sse_proxy.sh`
- 安装后的 wrapper：`~/.codex/ox_sse_proxy_launchd.sh`
- plist：`~/Library/LaunchAgents/<label>.plist`
- 日志：`~/.codex/ox_sse_proxy.log`
- pid：`~/.codex/ox_sse_proxy.pid`

本机已由 `bash scripts/install_launchd.sh install` 完成真实 Rust launchd 替换；安装后的
`~/.codex/ox_sse_proxy.sh check` 确认 `18899` listener 为
`$HOME/.codex/ox-sse-proxy 18899 https://opencode.ai/zen/go/v1`，不是 Python。
安装后 stop 确认 job/端口消失，start 和 restart 均生成新 PID 且 check 通过；最终重启后
真实冒烟返回 `SMOKE_OK`，TTFB `71957.7ms`，事件包含 created/delta/completed/ping。
安装备份位于 `$HOME/.codex/ox-sse-proxy-backups/<timestamp>`，回滚命令为：

```bash
bash scripts/install_launchd.sh restore latest
```

安装阶段真实冒烟 TTFB 为 `81728.2ms`；高 TTFB 是真实 OpenCode 上游波动，不能归因代理。

## Python 参考路径

旧 Python 参考实现保留在本仓库之外的私有项目中，路径不在此记录。
它只用于行为对照，不是本仓库 Rust 发布产物，也不应由新的 launchd wrapper 调用。
