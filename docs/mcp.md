# MCP 接入

## 配置模板

```json
{
  "transport": "sse",
  "url": "https://<relay-host>/agent/mcp/sse",
  "headers": { "X-Auth": "你的服务器密码" },
  "timeout": 60,
  "sse_read_timeout": 300
}
```

- `url` 只需要路径，无需查询参数
- `X-Auth` header 对应 relay 的 `--auth`（也可用 `?auth=` 查询参数，但 header 优先）
- Token 在每次工具调用时通过 arguments 动态传入，不进 URL 也不进 header

## 协议流程

```
GET  /agent/mcp/sse
  ← event: endpoint  /agent/mcp/messages?sessionId=xxx

POST /agent/mcp/messages?sessionId=xxx
  ← HTTP 202 Accepted

SSE  ← event: message  {JSON-RPC 响应}
```

符合 MCP SSE Transport 规范。

## 工具：shell_remote

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `token` | string | 是 | shell_remote Token（agent 会话 Token） |
| `cmd` | string | 是 | 要执行的 Shell 命令 |
| `timeout_ms` | number | 否 | 超时毫秒数（默认 30000，上限 300000，超出按 300000 截断） |
| `shell` | string | 否 | **仅 Windows 生效**：`auto`（默认）/ `powershell` / `cmd`。`auto` = 有 PowerShell 用 PowerShell，否则 cmd |
| `cwd` | string | 否 | 工作目录。缺省时子进程继承 agent 的当前目录（Windows 上默认为 agent 的 `--root`） |

调用示例：

```json
{
  "method": "tools/call",
  "params": {
    "name": "shell_remote",
    "arguments": {
      "token": "5fe42fc877b0a721...",
      "cmd": "cat /etc/hostname && uname -a"
    }
  }
}
```

- 只读 Token 调用本工具返回 JSON-RPC 错误 `-32002 Read-only token cannot call shell_remote`，并记入审计（`status: rejected_readonly`）
- relay 等待 `timeout_ms + 5s`：多出的 5 秒让 agent 自己的超时回答先到达，调用方因此能区分"命令超时"与"agent 没应答"
- 非零退出码以末行 `[exit code: N]` 报出，无输出时为 `Exit code: N`。非零退出码不是工具错误（`grep` 返回 1 是正常的）
- 每次调用都是新进程：`cd` 与环境变量改动不跨调用保留

## 登录环境（`/etc/profile`）

agent 由 systemd/服务直接启动，**不经过 PAM/SSH**，子进程只有 agent 自身的环境。设备上很多工具的 PATH 写在 `/etc/profile` 里（例如 Sophon 的 `bm-smi` 位于 `/opt/sophon/libsophon-current/bin`），所以网页终端和 MCP 默认都读不到它们——现在两条路径都以**登录 shell** 方式启动，不需要再手动 `source /etc/profile`：

| 路径 | 实际启动方式 |
|------|-------------|
| 网页终端 | `<--shell 指定的 shell> -l`（如 `/bin/bash -l`） |
| MCP `exec` / `exec_start` | `sh -l -c <你的命令>`（有 `script` 时由它分配 PTY，内层仍是这条 argv） |

- **不改写你的命令**：命令始终作为一个独立 argv 原样交给 `sh`，不拼接、不加前缀
- **不是 `ENV` / `BASH_ENV`**：POSIX 的 `ENV` 只在**交互** shell 里被读取，`ENV=/etc/profile sh -c '...'` 拿不到任何变量，因此走 `-l` 这一登录 shell 标准语义
- **`/etc/profile` 不存在时不影响任何功能**：shell 自行跳过，命令照常执行
- **只影响 Unix**：Windows 行为不变（无该概念）
- **不注入 locale**：agent 仍然不设置 `LANG` / `LC_*`，目标机自己的 locale 原样生效
- **若 `--shell` 指定的 shell 不接受 `-l`**（如 `tcsh` / `elvish` / `nu`），agent 启动时会自动探测并退回原来的非登录启动，网页终端照常可用，只是不加载 `/etc/profile`。探测结果按 shell 路径缓存，每个进程只做一次

> ⚠️ 登录环境的固有语义：如果 `/etc/profile` 或其 `profile.d` 里有只适合交互登录的输出/命令（打印欢迎语、启动 agent、设置仅终端有意义的变量等），MCP 的结果里也可能看到这些输出。这是登录 shell 的正常行为，不是 bug；遇到干扰请调整 profile。

**`script` 的两种实现**：MCP 执行时若系统有 `script`，会用它分配 PTY。util-linux 的 `script` 需要 `-e` 才能透传命令退出码，而 **busybox 的 `script` 不认 `-e`**（收到未知选项直接 usage + exit 1），Alpine / 精简容器上很常见。agent 启动时会探测目标 `script`，不支持就不加 `-e`——命令照常执行，只是退出码恒为 0（与引入登录环境之前的行为一致）。

## MCP 端文件传输

大文件上传/下载走专用流式端点（不经 LLM context）：

```bash
curl -T localfile -H "X-SR-Token: <token>" "https://relay/agent/mcp/put?path=/remote/path"
```

```bash
curl -H "X-SR-Token: <token>" "https://relay/agent/mcp/get?path=/remote/path" -o localfile
```

Token 即 `shell_remote` 的会话 Token；上传需 rw Token，下载 rw/ro 均可。NAT 下两机间文件传输走此端点（relay 中继，不落盘）。小内容（配置/脚本）直接用 `shell_remote` + heredoc 写入。

**路径语义**：`path` 为绝对路径时按远程文件系统原样使用；相对路径（如 `foo.txt`）相对于 agent 的 `--root`（默认 `$HOME`）解析，与命令执行时的 cwd 基准不同，注意区分。

**断点续传**：下载支持 HTTP Range（`curl -H "Range: bytes=1000-" ...`），中断后可续传；`bytes=-n`（后缀区间）不支持，会返回完整文件。Range 响应为 `206`，带 `Content-Range` / `Content-Length`。

**大小限制**：应用层为流式传输、无内置上限，实际上限由部署方反向代理（nginx/openresty）的 `client_max_body_size` 决定，默认常见 50m。若需更大上传，提高代理配置即可：

```nginx
location /agent/mcp/put {
    client_max_body_size 200m;
}
```

**错误码**：`401` Token 无效；`400` 缺少/重复 `path` 参数、路径是目录、相对路径无法解析、缺 `Content-Length`；`403` 权限不足（ro Token 上传）；`404` 文件不存在；`416` Range 越界；`429` 写入限流；`500` 其他服务错误。所有错误响应均为 `{"error": "..."}` JSON。

## 命令日志

agent（`shell-remote agent` 与 `shell-remote-ui agent` 两个二进制）**默认**把执行过的命令按统一格式打进日志，方便事后追溯"这条机器上到底跑了什么"。两个来源格式对齐，单行、字段间两个空格：

```
[MCP] exec        sess=ABCDE  shell=auto  cwd=/root  timeout=30s  $ ls -la
[MCP] exec_start  sess=ABCDE  $ tail -f /var/log/syslog
[MCP] stdin       sess=ABCDE  id=1a2b3c4d  bytes=4  "yes\n"
[WEB] cmd         sess=ABCDE  tab=tab-1  $ git status
[WEB] cmd~        sess=ABCDE  tab=tab-2  $ systemctl restart nginx
```

| 字段 | 含义 |
|------|------|
| 动作名 | `exec` / `exec_start` / `stdin` 来自 MCP；`cmd` / `cmd~` 来自网页终端 |
| `sess=` | 设备会话 ID（后台看到的那个） |
| `shell=` / `cwd=` / `timeout=` | MCP 请求里显式指定的值；未指定写 `-`。超时整秒写 `30s`，非整秒写 `1500ms` |
| `id=` / `bytes=` | stdin 目标会话 ID 的前 8 位 + 原始字节数 |
| `tab=` | 网页终端标签页 ID |
| `$ ` 之后 | 命令文本，永远最后一个字段；换行/制表符/控制符转义成 `\n` `\t` `\x1b`，超过 1000 字符截断并追加 `…(+N chars)` |

`cmd~` 表示这一行在编辑过程中用过方向键、Tab 补全或历史检索——agent 侧只能按键重建，内容可能与实际执行的不完全一致。精确的命令请以 `exec` 系列（MCP）为准。

**关闭**：

```bash
./shell-remote agent --relay-url https://relay.example.com --no-cmd-log
```

**日志位置**：沿用 tracing 配置——默认输出到 stderr；设 `SR_LOG_DIR` 时写该目录下按小时滚动的文件；`shell-remote-ui` 的窗口 / TUI 交互模式会自动落到 `~/.shell-remote/`，避免刷屏污染界面。

**限制**

- 网页终端只有在 shell 提示符下的输入才记录。密码输入（`sudo` / `ssh` / `read -s`，表现为回显关闭 + 行缓冲）和 `vim` / `python` / `ssh` 等前台程序里的按键都不记录——前者防泄密，后者防噪声
- 上述判定在 Unix 上通过终端 termios 与前台进程组实现；**Windows 的 ConPTY 拿不到这些信息，所以 Windows agent 会记录全部终端输入**（包括密码输入）。在 Windows 上处理敏感操作时请加 `--no-cmd-log`

## MCP 命令审计

启用 relay 的 `--record-dir` 后，每次 MCP `shell_remote` 工具调用也会被审计（与终端录制同开关、同目录），写入每会话文件 `{session_id}_{unix时间戳}.audit.jsonl`，每行一条 JSON：

```json
{"ts":"2026-07-06T12:34:56Z","unix_ms":1783339200000,"session_id":"seSupportBot","token_prefix":"b4e2ec42","permission":"rw","cmd":"ls -la","timeout_ms":30000,"duration_ms":12,"status":"ok","exit_code":0,"stdout_len":1234,"stderr_len":0,"stdout":"...","stderr":"..."}
```

- `status`：`ok` / `timeout` / `disconnected` / `no_agent` / `rejected_readonly`
- `token_prefix` 只记 Token 前 8 位（足以与后台 Token 列表对照，不泄露完整密钥）
- `stdout` / `stderr` 截断到 4KB（`stdout_len` / `stderr_len` 为完整长度）
- 管理后台概览页每会话显示 `●AUDIT` 标记；踢出/空闲回收会话时审计文件自动 flush 关闭
- 后台"录像"面板以"MCP 命令"类型列出每份审计文件，点击查看结构化的命令卡片（命令、状态、退出码、耗时、权限、Token 前缀、stdout/stderr），亦可直接删除
- **审计文件同样可能含命令及输出中的敏感内容，请保护录制目录的文件权限**
