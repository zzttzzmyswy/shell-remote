# shell-remote

简体中文 | [English](README_en.md)

自托管、轻量级的远程服务器协作工具。单个 Rust 二进制文件，即可通过浏览器共享终端会话、管理远程文件，并为 AI Agent 暴露 MCP 协议接口。

## 功能

- **协同终端** — 多人通过浏览器同时查看和操作同一个 Shell 会话（xterm.js + WebGL 渲染）
- **多 Tab 独立** — 每位用户独立切换多个 PTY Shell 标签页，互不干扰
- **文件管理器** — 侧栏面板，面包屑导航、上传、下载、删除、重命名、新建文件夹、刷新
- **MCP 服务器** — AI Agent（Claude 等）通过标准 MCP SSE Transport 协议在远程机器上执行命令
- **SSE+POST 协议** — 全链路使用 HTTP SSE 推送 + POST 发送，兼容性好，不依赖 WebSocket
- **单二进制** — 所有 Web 资源通过 `rust-embed` 编译嵌入，零外部文件依赖
- **Token 鉴权** — 随机临时 Token 或固定密钥；支持读写和只读两种权限
- **服务器密码** — Relay 可配置访问密码（`--auth`），必填

## 架构

```
浏览器 (xterm.js + 文件管理UI)
         │ SSE + POST /agent/session/sse + /agent/session/send
         ▼
┌───────────────┐   HTTP SSE+POST (/agent/events + /agent/send)   ┌──────────────┐
│   Relay       │ ◄─────────────────────────────────────────────► │   Agent      │
│   路由 + 鉴权  │                                                  │   Shell + FS │
│   静态 + MCP  │                                                  │   (目标机器)  │
└───────────────┘                                                  └──────────────┘
         ▲
         │ MCP (/agent/mcp/sse + /agent/mcp/messages)
         │
   AI Agent (Claude 等)
```

- **Relay**：消息路由中心，连接各方并执行权限检查；嵌入 Web 前端
- **Agent**：在目标机器上运行，管理 PTY Shell 和文件系统

## 快速开始

### 下载预编译二进制

[GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases) 提供三种架构的 musl 静态编译二进制：

```bash
# x86_64 (Intel/AMD)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64 && chmod +x shell-remote-x86_64

# aarch64 (ARM 64位, 树莓派4/5, 云服务器)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-aarch64 && chmod +x shell-remote-aarch64

# armv7 (ARM 32位, 树莓派2/3)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-armv7 && chmod +x shell-remote-armv7
```

### 编译

```bash
git clone https://github.com/zzttzzmyswy/shell-remote.git && cd shell-remote
cargo build --release
```

### 启动 Relay

```bash
# --auth 必填；TLS 由前端反向代理（nginx/caddy）终结
./shell-remote relay --auth YourStrongPassword --bind 0.0.0.0:3000
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--bind` | `0.0.0.0:3000` | 监听地址 |
| `--auth` | 无默认值 | 服务器密码（必填） |
| `--record-dir` | 无 | 终端会话录制目录（asciinema cast v2）；不设则不录制 |

### 启动 Agent

```bash
./shell-remote agent --relay-url https://<relay-ip>
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--relay-url` | `https://localhost:3000` | Relay 地址（HTTPS 或 HTTP，使用 SSE+POST 协议） |
| `--key` | — | 固定鉴权密钥（不指定则随机生成临时 Token） |
| `--root` | `$HOME` | 文件管理器默认目录 |
| `--token-type` | `rw` | Token 类型：`rw`、`ro` 或 `both` |
| `--shell` | `/bin/bash` | Shell 路径 |
| `--session-id` | — | 自定义会话 ID（5-20 位字母数字），后台据此区分设备；冲突则启动失败 |

输出示例：

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

- `session:` — 8 位会话 ID（仅用于日志）
- `rw:` / `ro:` — Token（浏览器登录或 MCP 调用使用）

### 浏览器访问

打开 `http://<relay-ip>:3000`，输入服务器密码及 Token 即可连接。主区域为 xterm.js 终端，右侧为文件管理器。

## Windows Agent

系统要求：Windows 10 1809+（ConPTY 最低版本）。

一行命令安装并运行（relay 地址自动注入）：

```powershell
# 默认 cmd
irm http://your-relay:3000/agent/install.ps1 | iex

# 或手动下载 release 中的 shell-remote-x86_64.exe 重命名为 shell-remote.exe
```

仅下载到当前目录不执行：

```powershell
& ([scriptblock]::Create((irm http://your-relay:3000/agent/install.ps1))) --download-only
```

> Linux/macOS 等价命令：`curl -fsSL http://your-relay:3000/agent/install | sh`（运行）或 `... | sh -s -- --download-only`（仅下载）。

手动启动：

```powershell
# 默认 cmd
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx

# 使用 PowerShell
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx --shell powershell.exe
```

注意事项：建议以管理员身份运行以完整访问文件系统；交互式程序（ssh/vim）在 MCP exec 路径暂不支持；文件上传（upload）在 Windows agent 暂不支持，请用 `fs:write`（base64）替代，文件下载（read）正常。

### Windows 交叉编译（从 Linux）

```bash
rustup target add x86_64-pc-windows-gnu
# 需 x86_64-w64-mingw32-gcc（mingw-w64）
cargo build --release --target x86_64-pc-windows-gnu
```

### 功能对比

| 功能 | Linux/macOS | Windows |
|------|-------------|---------|
| PTY 交互式 Shell | ✅ | ✅（ConPTY） |
| 命令执行 | ✅ | ✅（cmd / pwsh） |
| 文件浏览/读写/改名/删除 | ✅ | ✅ |
| 文件下载（read） | ✅ | ✅ |
| 文件上传（upload） | ✅ | ❌（用 fs:write 替代） |
| 交互式程序(ssh/vim) | ✅ | ⚠️ exec 路径不支持 |
| 文件权限位(mode) | ✅ | 显示占位（无 POSIX 权限） |

## API 端点

所有端点统一在 `/agent` 路径下：

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/session/sse` | GET → SSE | 浏览器连接 Relay 接收消息流 |
| `/agent/session/send` | POST | 浏览器发送消息 |
| `/agent/events` | GET → SSE | Agent 接收消息流（HTTP 模式） |
| `/agent/send` | POST | Agent 发送消息（HTTP 模式） |
| `/agent/upload` | POST | 文件上传 |
| `/agent/mcp/sse` | GET → SSE | MCP SSE Transport 端点 |
| `/agent/mcp/messages` | POST | MCP JSON-RPC 消息 |

## 管理后台

Relay 可选启用一个 web 管理后台：查看会话/Token、踢出会话、管理 Token 权限、查看/修改服务器密码、查看运行时状态。默认禁用，需命令行显式开启；**首页不显示入口，必须手动输入秘密子路径才能到达**。

### 启用

```bash
shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 \
  --admin-path /your-secret-path --admin-pass ADMIN_PASSWORD
# --admin-user 默认 "admin"；不设 --admin-path 则后台完全不可访问
```

### 访问

浏览器打开 `http://<relay-ip>:3000/your-secret-path`（即 `--admin-path` 的值），输入 `--admin-user` / `--admin-pass` 登录。秘密路径是第一道屏障；登录后获发 HttpOnly + SameSite=Strict 的 session cookie（12h 有效）。

### 功能

- **概览**：版本、运行时间、agent 总数/在线数、浏览器总数、每会话 Token 列表与权限、连接浏览器数。
- **Token 管理**：撤销单个 Token、重生成会话 Token（旧 Token 失效）、切换 Token 权限（rw↔ro）。
- **会话标签**：给已有会话打标签（如 prod/db），按标签筛选；标签内存级，随会话存亡。
- **跳转终端**：每会话"连接"按钮，新标签页打开该会话的浏览器终端（token 预填，服务器密码仍需手填）。
- **会话录制**：`--record-dir` 启用后，交互式终端 I/O（输出+输入）以 asciinema cast v2 落盘；后台显示录制状态，文件可用 `asciinema play` 回放。
- **踢出会话**：断开该 agent 及其所有浏览器并撤销其 Token。
- **服务器密码**：查看当前 `--auth`、在线修改（即时生效）。
- **中英文切换**：后台界面右上角切换中/英文（自动探测浏览器语言，localStorage 记忆）。

### 安全说明

- 后台页面不在公开静态资源目录，无法经 `/admin.html` 等路径访问。
- 双层保护：秘密路径（隐藏入口）+ 账户密码登录。
- admin session 仅存内存，relay 重启需重新登录。
- 已知局限：撤销/重生成 Token 后，若 agent 用 `register_existing`（带 Token 重连）重连，可能重新带回该 Token；不影响在线会话。

## 会话录制

relay 加 `--record-dir <目录>` 即可录制交互式终端会话（不含 MCP exec）：

```bash
shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 --record-dir /var/log/shell-remote
```

- 格式：asciinema cast v2（JSONL），可用 `asciinema play xxx.cast` 或 xterm.js 回放。
- 录制输出 + 输入流；**输入流会包含终端里键入的敏感内容（如 sudo 密码），请妥善保护录制目录的文件权限**。
- 每会话一个文件 `{session_id}_{unix时间戳}.cast`；agent 用 `--session-id` 指定后文件名即该 ID。
- 录制在 relay 侧捕获，不影响 agent；踢出/空闲回收会话时文件自动 flush 关闭。

## AI Agent 接入 (MCP)

### 配置模板

```json
{
  "transport": "sse",
  "url": "https://<relay-host>/agent/mcp/sse",
  "headers": { "X-Auth": "你的服务器密码" },
  "timeout": 60,
  "sse_read_timeout": 300
}
```

- `url`：只需要路径，无需查询参数
- `X-Auth` header：服务器密码（对应 relay 的 `--auth`）
- Token 在每次工具调用时通过 arguments 动态传入

### 协议流程

```
GET  /agent/mcp/sse
  ← event: endpoint  /agent/mcp/messages?sessionId=xxx

POST /agent/mcp/messages?sessionId=xxx
  ← HTTP 202 Accepted

SSE  ← event: message  {JSON-RPC 响应}
```

符合 MCP SSE Transport 规范。

### 唯一工具：shell_remote

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `token` | string | 是 | shell_remote token（Agent 会话 Token） |
| `cmd` | string | 是 | 要执行的 Shell 命令 |
| `timeout_ms` | number | 否 | 超时毫秒数（默认 30000，最大 300000） |

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

- `token` 在 arguments 中传入，不在 URL 或 Header
- `cmd` 通过 `sh -c` 执行，支持管道、重定向等完整 Shell 语法
- 返回 stdout、stderr 和 exit code

## Token 权限模型

| Token 类型 | 终端输入 | 文件操作 | MCP 执行 |
|-----------|---------|---------|----------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly | ❌ | 列表/读取 | ❌ |

- **临时 Token**：Agent 断开即失效
- **固定密钥**：通过 `--key` 指定，Agent 重连后仍可使用

## 文件管理器

- 面包屑路径导航
- 上传（流式传输，默认 100MB 上限）
- 下载、删除、重命名、新建文件夹、刷新
- 侧栏宽度可拖拽调整

## 技术栈

| 层 | 技术 |
|----|------|
| 运行时 | Rust + Tokio 异步 |
| HTTP | Axum |
| 终端 | portable-pty + xterm.js |
| 静态嵌入 | rust-embed |
| 前端 | 原生 HTML/CSS/JS |
| MCP | SSE Transport + JSON-RPC |

## 测试

```bash
cargo test
# 150 passed; 0 failed (含集成测试)
```

## 许可证

MIT
