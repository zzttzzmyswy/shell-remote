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

输出示例：

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

- `session:` — 8 位会话 ID（仅用于日志）
- `rw:` / `ro:` — Token（浏览器登录或 MCP 调用使用）

### 浏览器访问

打开 `http://<relay-ip>:3000`，输入服务器密码及 Token 即可连接。主区域为 xterm.js 终端，右侧为文件管理器。

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

### 唯一工具：exec_remote

| 参数 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `token` | string | 是 | Agent 会话 Token |
| `cmd` | string | 是 | 要执行的 Shell 命令 |
| `timeout_ms` | number | 否 | 超时毫秒数（默认 30000，最大 300000） |

调用示例：

```json
{
  "method": "tools/call",
  "params": {
    "name": "exec_remote",
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
# 106 passed; 0 failed (含集成测试)
```

## 许可证

MIT
