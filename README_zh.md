# shell-remote

[English](README.md) | 简体中文

自托管、轻量级的远程服务器协作工具。部署单个 Rust 二进制文件，即可通过浏览器共享终端会话、管理远程文件，并为 AI Agent 暴露 MCP 协议接口。

## 功能

- **协同终端** — 多人通过浏览器同时查看和操作同一个 Shell 会话（xterm.js + WebGL 渲染）
- **多 Tab 独立** — 每位用户独立切换多个 PTY Shell 标签页，互不干扰
- **文件管理器** — 侧栏面板，面包屑导航、上传（HTTP 流式，无大小限制）、下载、删除、重命名、新建文件夹、刷新
- **MCP 服务器** — AI Agent（Claude 等）通过标准 MCP 协议在远程机器上执行命令、管理文件
- **双传输协议** — Agent 支持 WebSocket (`ws://`) 和 HTTP SSE+POST (`https://`) 两种模式，兼容任意 HTTP/1.1/2/3 反向代理
- **单二进制** — 所有 Web 资源通过 `rust-embed` 编译嵌入，零外部文件依赖
- **Token 鉴权** — 随机临时 Token（默认）或固定密钥；支持读写和只读两种权限
- **服务器密码** — Relay 可配置访问密码（`--auth`），保护 Web 界面

## 架构

```
浏览器 (xterm.js + 文件管理UI)
        │ WebSocket /agent
        ▼
┌───────────────┐   WS (/agent) 或 HTTP (SSE+POST)   ┌──────────────┐
│   Relay       │ ◄─────────────────────────────────► │   Agent      │
│   路由 + 鉴权  │                                      │   Shell + FS │
│   静态 + MCP  │                                      │   (目标机器)  │
└───────────────┘                                      └──────────────┘
        ▲
        │ MCP (/agent/mcp/sse + /agent/mcp/messages)
        │
  AI Agent (Claude 等)
```

- **Relay**：无状态消息路由器，连接各方并执行权限检查；嵌入 Web 前端
- **Agent**：在目标机器上运行，管理 PTY Shell 和文件系统；支持 WebSocket 和 HTTP SSE+POST 两种传输

## 快速开始

### 编译

```bash
# 需 Rust 1.75+
git clone https://github.com/zzttzzmyswy/shell-remote.git && cd shell-remote
cargo build --release
```

产物为 `target/release/shell-remote` 单个静态二进制。

### 下载预编译二进制

[GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases) 提供三种架构的 musl 静态编译二进制：

```bash
# x86_64 (Intel/AMD)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64 && chmod +x shell-remote-x86_64

# aarch64 (ARM 64位, 树莓派4/5)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-aarch64 && chmod +x shell-remote-aarch64

# armv7 (ARM 32位, 树莓派2/3)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-armv7 && chmod +x shell-remote-armv7
```

或访问任意运行中的 Relay 的 `/download` 页面，提供平台检测和可复制的 curl 命令。

跨平台编译指南见 [BUILD.md](BUILD.md)（支持 x86_64、aarch64、armv7 三架构）。

### Docker

```bash
docker build -t shell-remote .
docker run -d --name shell-remote-relay -p 3000:3000 shell-remote relay --dev --bind 0.0.0.0:3000
```

### 启动 Relay

```bash
./shell-remote relay --dev --bind 0.0.0.0:3000
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--bind` | `0.0.0.0:3000` | 监听地址 |
| `--dev` | false | 开发模式（明文 WebSocket） |
| `--auth` | `password` | 服务器访问密码 |
| `--bin-dir` | — | 预编译二进制文件目录（供 /download 页面使用） |
| `--tls-cert` | — | TLS 证书路径 |
| `--tls-key` | — | TLS 私钥路径 |

### 启动 Agent

```bash
# WebSocket 模式（实时双向，延迟最低）
./shell-remote agent --relay-url ws://<relay-ip>:3000 --root /home/user

# HTTP SSE+POST 模式（兼容 HTTP/2/3 和任意反向代理）
./shell-remote agent --relay-url https://<relay-ip> --root /home/user
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--relay-url` | `ws://localhost:3000` | Relay 地址。`ws://`=WebSocket，`https://`=SSE+POST |
| `--key` | — | 固定鉴权密钥（不指定则随机生成） |
| `--root` | `$HOME` | 文件浏览器起始目录 |
| `--token-type` | `rw` | Token 类型：`rw`、`ro` 或 `both` |
| `--shell` | `/bin/bash` | Shell 路径 |

输出：

```
session: a1b2c3d4
  rw: e83f2a1b9c...
```

- `session:` — 8 位会话 ID（仅用于日志标识）
- `rw:` / `ro:` — 身份验证 Token（浏览器登录使用）

### 浏览器访问

打开 `http://<relay-ip>:3000`，输入服务器密码及 Token，点击连接即可。

- 主区域：xterm.js 终端，WebGL 加速渲染
- 右侧抽屉：文件管理器，支持面包屑导航、上传下载、右键菜单

多位用户使用相同 Token 可同时加入同一会话，实时共享终端输出，各自独立切换 Tab。

## API 端点

所有客户端端点统一在 `/agent` 路径下：

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent` | GET → WS | 浏览器和 Agent WebSocket 连接 |
| `/agent/events` | GET → SSE | Agent 接收消息流（HTTP 模式） |
| `/agent/send` | POST | Agent 发送消息（HTTP 模式） |
| `/agent/upload` | POST | 文件上传（Bearer Token 鉴权） |
| `/agent/mcp/sse` | GET → SSE | MCP SSE 端点 |
| `/agent/mcp/messages` | POST | MCP JSON-RPC 消息 |

## AI Agent 接入 (MCP)

Relay 暴露 MCP 协议端点：

- SSE：`http://<relay-ip>:3000/agent/mcp/sse?token=<token>`
- 消息：`http://<relay-ip>:3000/agent/mcp/messages`（Token 通过 `Authorization: Bearer <token>` 头传递，query 参数 `?token=` 作为降级兼容）

### MCP 工具列表

所有工具均以 `remote_` 前缀命名，避免与 AI Agent 自身工具冲突。

| 工具 | 参数 | 说明 |
|------|------|------|
| `exec_remote` | `cmd`, `timeout_ms?` | 执行一次性 Shell 命令（30s 超时） |
| `exec_remote_start` | `cmd` | 启动交互式命令会话，返回 `exec_id` 和初始输出 |
| `exec_remote_input` | `exec_id`, `data` | 向运行中的会话发送 stdin |
| `exec_remote_close` | `exec_id` | 关闭会话（进程在运行则 kill），返回最终输出和退出码 |
| `exec_remote_list` | — | 列出所有活跃执行会话 |
| `file_remote_read` | `path` | 读取文件（base64 编码） |
| `file_remote_write` | `path`, `content` | 写入文件内容 |
| `file_remote_list` | `path` | 列出目录内容 |

## Token 权限模型

| Token 类型 | 终端输入 | 文件操作 | MCP 执行 |
|-----------|---------|---------|----------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly | ❌ | 仅读（list/read） | ❌ |

- **临时 Token**：64 位随机十六进制字符串，Agent 断开即失效
- **固定密钥**：通过 `--key` 指定，Agent 重新连接后仍可使用

## 文件管理器

- 面包屑路径导航，每级可点击跳转
- 上传通过 HTTP POST 到 `/agent/upload`（`Authorization: Bearer <token>` 请求头），流式传输，无大小限制
- 下载通过 WebSocket，`_mcp_request_id` 路由分发
- 删除、重命名、新建文件夹、刷新
- 侧栏宽度可拖拽调整
- 实时上传进度（XHR `upload.onprogress`）

## 技术栈

| 层 | 技术 |
|----|------|
| 运行时 | Rust + Tokio 异步 |
| HTTP/WS | Axum |
| 终端 | portable-pty + xterm.js (WebGL) |
| 字体 | Sarasa Term SC (jsDelivr CDN) |
| 静态嵌入 | rust-embed |
| 前端 | 原生 HTML/CSS/JS（无构建步骤） |
| MCP | HTTP SSE + JSON-RPC |

## 测试

```bash
cargo test
# 100 passed; 0 failed
```

## 许可证

MIT
