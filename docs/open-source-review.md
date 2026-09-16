# 开源前敏感信息与许可复核

复核日期：2026-09-17
复核范围：仓库内全部受版本控制的文件、完整 git 历史（`git rev-list --all`）、Cargo 依赖树
复核对象版本：`7455ff1` 及本次工作区改动

## 结论

**可以开源。** 未发现任何真实凭据或机密材料；发现的个人可识别信息已在本轮清理；全部
依赖均为宽松许可，可自由再分发。剩余事项见第四节，均需项目所有者本人确认，而不是技术
阻塞。

## 一、凭据与机密材料：未发现

按 `api_key`、`secret`、`token`、`password`、`credential`、`bearer`、`authorization`、
`private_key`、`AKIA*`、`sk-*`、`ghp_*` 等模式扫描了全部受控文件和全部 git 历史提交。
所有命中都属于以下无害类别：

| 命中示例 | 位置 | 性质 |
| --- | --- | --- |
| `OPENCODE_GO_API_KEY` | `README.md`、`docs/`、`scripts/smoke_real_proxy.py` | 环境变量名；脚本只读取进程环境，不写文件、不回显 |
| `sk-your-token-placeholder`、`sk-...` | `README.md`、`ox_sse_proxy_design.md` | 文档占位符 |
| `Bearer public` | `src/service.rs`、设计文档 | 上游免费通道的公开固定值，不是私有凭据 |
| `session=secret`、`api-secret`、`Basic secret` | `tests/proxy_service.rs` | 单元测试构造的假 header，用于断言代理**不会**转发入站凭据 |
| `query-secret-should-not-leak` | `tests/proxy_service.rs` | 泄漏防护测试的合成字符串 |
| `max_tokens` / `*_tokens` | 源码、测试 | 计数字段名，非凭据 |

其他确认项：

- 无 `.env`、密钥文件、证书、keystore、cookie jar 或凭据转储被跟踪；`.gitignore`
  已覆盖 `.env*`、`secret/`、`secrets/`、`*.key`。
- `git fsck` 无 dangling 对象；历史只有一个提交，没有已删除但仍可恢复的机密文件。
- 运行时密钥只经环境变量注入，日志按设计只记录脱敏 host/path。

## 二、个人可识别信息：已在本次清理

原始提交中包含与本机用户绑定的字符串。它们不是凭据，但对一个开源仓库来说是噪声，
且会泄露开发者的本机布局与私有项目名。已完成的整改：

| 位置 | 原内容 | 现状 |
| --- | --- | --- |
| `scripts/*.sh`、plist 模板 | launchd label 与 plist 文件名中嵌入了个人用户名（`com.<用户名>.ox-sse-proxy`） | label 参数化为 `OX_PROXY_LABEL`，默认 `com.ox-sse-proxy`，安装时渲染进 plist 与 control 脚本 |
| `scripts/install_launchd.sh` | `backup_name plist` 写死文件名 | 跟随 `$LABEL` 动态生成，`restore` 放宽为接受任意 `*.plist` 备份名 |
| `README.md`、`docs/`、设计文档 | 含个人用户名的绝对路径（`/Users/<用户名>/...`） | 改为 `$HOME/...` 或 `<timestamp>` 占位 |
| 设计文档、外部资源文档 | 另一个私有项目的绝对路径（含项目名与脚本名） | 改为「本仓库之外的私有项目」，不再暴露项目名与路径 |

迁移行为：`install` 会扫描 `~/Library/LaunchAgents/`，自动停用其他仍指向本项目
`ox_sse_proxy_launchd.sh` 的已加载 job，因此从旧 label 升级不需要手工 `launchctl` 操作。

上述整改已随历史重写生效：仓库当前只有一个提交，原始提交不再出现在任何分支、tag 或
release 中。本机 clone 的 reflog 仍可能保留旧对象（默认 90 天），如不需要本地回滚能力，
可执行 `git reflog expire --expire=now --all && git gc --prune=now` 清除。

## 三、依赖许可：全部宽松，可再分发

`cargo metadata` 覆盖 179 个依赖 crate，全部声明了许可，无缺失字段：

| 许可 | 数量 |
| --- | --- |
| MIT OR Apache-2.0 | 93 |
| MIT | 32 |
| Unicode-3.0 | 18 |
| Apache-2.0 OR MIT | 10 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 5 |
| Apache-2.0 OR ISC OR MIT | 2 |
| MIT OR Apache-2.0 OR Zlib | 2 |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later（`r-efi`，可择 MIT） | 2 |
| ISC | 2 |
| 其他单项宽松许可（BSD、Zlib、Unlicense、CDLA-Permissive-2.0 等） | 13 |

不存在 GPL/AGPL/SSPL 等强制 copyleft 依赖，因此本项目可以按 **MIT OR Apache-2.0**
双许可分发。该组合与 Rust 生态一致，也是本仓库采用它的原因。

## 四、需要项目所有者确认的事项

1. **提交作者邮箱（已处理，但有残留窗口）。** 原始提交使用真实个人邮箱署名，已通过
   重写历史改为 GitHub noreply 地址，仓库历史现在只有一个提交。注意：被替换的旧提交
   对象虽然已脱离分支历史，但在 GitHub 完成对象回收前，仍可能通过 commit SHA 直链读取。
   若需要立即彻底清除，只能删除仓库重建，或向 GitHub Support 请求回收不可达对象。
2. **第三方服务条款与商标。** 本项目对接 `opencode.ai` 的 Zen/Go 网关，并在免费通道
   使用其公开的 `Bearer public`。请自行确认这样使用与再分发符合该服务的使用条款；
   README 中的产品名仅作事实描述，不声称任何关联或背书。
3. **crates.io 发布。** `Cargo.toml` 目前设置 `publish = false` 以避免误发布。如果
   需要发布到 crates.io，请删除该行，并按 crates.io 要求补全元数据。
4. **许可署名。** `LICENSE-MIT` 使用 `Copyright (c) 2026 ZhongGheart`。如需改用真实
   姓名或组织名，替换该行即可（Apache-2.0 正文本身不含版权行）。

## 五、可重复执行的复核命令

```bash
# 凭据模式扫描（受控文件）
git ls-files -z | xargs -0 grep -nEi \
  '(api[_-]?key|secret|token|passwor|credential|private[_-]?key|sk-[A-Za-z0-9]{16,}|ghp_|AKIA[0-9A-Z]{12,})'

# 个人可识别信息残留（含未跟踪文件，排除 .git 与 target）
grep -rnEi '/Users/[a-z0-9._-]+|com\.[a-z0-9]+\.ox-sse-proxy' \
  --exclude-dir=.git --exclude-dir=target .

# 依赖许可清单
cargo metadata --format-version 1 --locked \
  | python3 -c "import json,sys;print(sorted({(p.get('license') or 'MISSING') for p in json.load(sys.stdin)['packages'] if p.get('source')}))"

# 安装资产静态检查
bash -n scripts/*.sh
python3 -m py_compile scripts/smoke_real_proxy.py
tmp_plist="$(mktemp)"
sed -e "s|__HOME__|$HOME|g" \
  -e "s|__LABEL__|${OX_PROXY_LABEL:-com.ox-sse-proxy}|g" \
  scripts/com.ox-sse-proxy.plist.in > "$tmp_plist"
plutil -lint "$tmp_plist" && rm -f "$tmp_plist"
```
