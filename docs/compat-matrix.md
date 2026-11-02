# 跨版本兼容矩阵

`tools/compat_matrix.sh` 用**真实的已发布旧二进制**验证两个方向的兼容性：

| 组合 | relay | agent | 验证的承诺 |
|------|-------|-------|-----------|
| **A** | 新版本 | 旧版本 | 新 relay 不能踢掉存量旧 agent（升级 relay 时线上大量旧客户端不能掉线） |
| **B** | 旧版本 | 新版本 | 新 agent 必须能注册到还没升级的旧 relay（用户装新版 agent、relay 没跟上） |

它不像 `tools/*.sh` 里其它脚本那样两个角色都用同一个二进制——那样只能证明「同版本自洽」，发现不了协议两侧的漂移。

## 用法

```bash
cargo build --release                     # 先产出新二进制
tools/compat_matrix.sh                    # 默认版本列表
tools/compat_matrix.sh --versions "0.53.0 0.59.8"   # 只跑指定版本
tools/compat_matrix.sh --new dist/shell-remote-x86_64
```

| 参数 | 默认 | 说明 |
|------|------|------|
| `--new` | `target/release/shell-remote` | 新二进制；必须能被 `--version` 读出 0.59.11 这样的版本号 |
| `--versions` | `0.53.0 0.59.4 0.59.8 latest` | 要测的旧版本。`latest` 取 GitHub 最新 release，**等于新二进制版本时自动跳过**（两方向退化成同版本，没有意义） |

退出码：全部 `PASS`/`GAP` 为 `0`，任意一项 `FAIL` 为 `1`（可直接用于 CI 与发版前置检查）。

## 检查项

每个版本 × 每个组合跑 6 项：

| 检查项 | 做法 | 通过条件 |
|--------|------|----------|
| `agent_register` | 起 relay + agent | 浏览器 SSE 握手 200（relay 只在该会话的 agent 通道已建立时才放行）、relay 日志有 `session created`、agent 日志有 `agent session established`，且 admin overview 里该会话 `online=true` |
| `mcp_exec` | MCP 的 SSE + `messages` 走一次 `tools/call`，执行 `echo compat-中文-ok` | 返回文本与期望**一字不差**（同时覆盖 UTF-8 与 MCP 传输本身） |
| `file_transfer` | `PUT /agent/mcp/put` 传 64KB 随机文件 → `GET /agent/mcp/get` 取回 | 两次都 200，且往返 `sha256` 一致 |
| `web_terminal` | 浏览器 SSE → 从 `session:tab_list` 取 tab → `POST /agent/session/send` 发 `terminal:input` 执行 `echo term-ok` | 在 `terminal:output` 里剥掉 OSC/CSI 转义后读到一行恰好等于 `term-ok` |
| `reconnect` | kill relay → 同端口同参数重启 | 60s 内 agent 重新注册（SSE 再次 200），且**再跑一次 `mcp_exec` 通过** |
| `cleanup` | 收尾杀进程 | 用本组合独有的 `--session-id` 与端口过滤 `pgrep`，无残留 |

所有端点都是 relay 的公开 HTTP 接口，没有 mock 内部状态。探针
（`tools/compat/sr_probe.py`）只用 Python 标准库。

### 参数自适应

不同版本的 CLI 参数并不一样（例如 TLS 相关开关）。脚本对每个二进制先跑
`relay --help` / `agent --help`，只在参数存在时才传：`--no-tls`、
`--admin-path`、`--admin-pass`、`--session-id`、`--shell`、`--relay-insecure`。
不要改成写死——新增版本时很可能又缺某个开关。

## 旧二进制从哪来

- 下载 `https://github.com/<repo>/releases/download/v<ver>/shell-remote-x86_64`
- 先直连 github.com，失败再走 `https://gh-proxy.com/<完整 URL>`
- 缓存到 `${XDG_CACHE_HOME:-$HOME/.cache}/shell-remote-compat/v<ver>/`，命中即复用
- **校验**：用 `gh release view v<ver> --json assets` 里的 `digest` 比对 SHA256；
  该资产没有 digest 时记录「未校验」并在报告的「各版本二进制来源」表里标出

删缓存重下：`rm -rf ~/.cache/shell-remote-compat/v0.59.8`。

## 输出

- 终端打印矩阵表（版本 × 组合 × 检查项，`PASS` / `FAIL` / `GAP`）与总耗时
- 当前目录写一份 `compat-report.md`（矩阵、GAP 与原因、FAIL 与日志目录、各版本二进制的 SHA256 与校验状态）
- 每个组合的 relay/agent 日志留在 `mktemp -d` 出来的临时目录里，路径在结尾打印；失败时终端也会给出该目录

## 如何新增版本

1. 确认该 tag 的 release 里有 `shell-remote-x86_64` 资产。
2. 加进默认列表，或临时用 `--versions` 跑：
   ```bash
   tools/compat_matrix.sh --versions "0.52.0"
   ```
3. 有检查项失败时，先判断是**真实兼容 bug**还是脚本问题（见下一节）。

`latest` 会在每次发布新 tag 后自动跟随，因此新版本发布后即使没人改脚本，
矩阵也会自动把「新二进制 vs 上一版」纳入覆盖。

## 豁免规则（`tools/compat/known-gaps.txt`）

只有当某项检查需要的**功能在该版本里本来就不存在**时才允许登记豁免，
格式为一行一条：

```
<版本> <组合> <检查项> <原因>
0.51.0 both file_transfer v0.51.0 尚未引入 MCP 文件传输端点 /agent/mcp/put，见 v0.52.0 release note
```

- 组合：`A` / `B` / `both`；检查项：上表六个之一，或 `*`
- 原因**必须带可验证的证据**（release tag、commit、源码位置）

**不允许为了让矩阵变绿而登记豁免。** 豁免是记录「旧版本本来就没有」，不是
掩盖新版本改坏的行为。真实失败要修代码，或者在评审里明确接受这个兼容性回退。

## 发现 FAIL 时

1. 看终端给出的日志目录：`relay-1.log` / `relay-2.log` / `agent-1.log` /
   `mcp.err` / `xfer.err` / `term.err` / `reconnect.note` / `leftover.txt`。
2. 分辨是哪一侧的问题：
   - 只在 **A** 失败 → 新 relay 对旧 agent 的行为变了；
   - 只在 **B** 失败 → 新 agent 依赖了旧 relay 没有的东西；
   - 两个方向都失败 → 多半是脚本/环境问题，先查探针。
3. 真实兼容 bug 不要在这张卡里改 `src/`，按复现步骤 + 日志 + 推测位置报出来。
