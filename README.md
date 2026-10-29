# shell-remote

简体中文 | [English](README_en.md)

自托管、轻量级的远程协作工具：单个 Rust 二进制即可通过浏览器共享终端会话、管理远程文件、共享桌面，并为 AI Agent 暴露 MCP 接口。所有 Web 资源经 `rust-embed` 编译进二进制，部署零外部文件依赖。

## 功能特性

- **协同终端** — 多人通过浏览器同时查看和操作同一 PTY 会话（xterm.js + WebGL），每人独立切换自己的标签页
- **弱网续接** — PTY 跨 relay 重连存活，断网 / relay 重启 / 半开链路恢复后正在运行的命令不中断，重新加入时回放各标签页最近 64KB 输出；上下行都有超时与心跳判死。详见 [docs/performance.md](docs/performance.md)
- **桌面共享** — 捕获 X11/Windows 桌面，AV1（libaom）或 H.264（openh264）纯软件编码，码率自适应；下行按 LAN → WebRTC P2P → relay 顺序自动选路，用户无感。详见 [docs/desktop.md](docs/desktop.md)
- **网页映射** — 会话页"网页"按钮经 relay/agent 隧道浏览远端设备网络中的页面（含设备自身 `127.0.0.1` 服务）。v2 完整模式支持脚本、SPA 路由、Cookie 登录、POST 与 WebSocket；v1 静态模式只渲染静态页面。详见 [docs/web-map.md](docs/web-map.md)
- **MCP 服务器** — AI Agent 通过标准 MCP SSE Transport 在远端执行命令，另有独立流式端点传输大文件，字节不过 LLM context。详见 [docs/mcp.md](docs/mcp.md)
- **文件管理器与会话录制** — 面包屑导航、上传下载改名删除；asciinema cast v2 录制与 MCP 命令审计。详见 [docs/sessions.md](docs/sessions.md)
- **管理后台** — 概览 / 会话 / 设备 / 录像 / 访问日志 / 设置六个标签页；设备信息探测、一键连接、一键原子自升级。详见 [docs/admin.md](docs/admin.md)
- **SSE + POST 传输** — 全链路 HTTP 推送 + POST 发送，不依赖 WebSocket，兼容任意反向代理
- **两种二进制** — `shell-remote`（lean，仅终端与 MCP）与 `shell-remote-ui`（终端 + 桌面），共享同一 relay 与协议，可接入同一 relay

## 架构

```
浏览器 (xterm.js + 文件管理 UI)
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

- **Relay** — 消息路由中心，执行鉴权与权限检查，并内嵌 Web 前端与 MCP 服务
- **Agent** — 在目标机器上运行，管理 PTY Shell 与文件系统；桌面共享另需 `shell-remote-ui`

| 二进制 | 子命令 | 能力 | 典型用途 |
|--------|--------|------|----------|
| `shell-remote` | `relay` / `agent` | 终端转发（PTY + 文件管理器 + MCP exec），构建不含桌面代码 | 自建 relay；NAS / 云主机 / 无显示器设备的 agent；MCP 集成 |
| `shell-remote-ui` | `agent` | 终端 + 桌面转发（含全部 `--desktop-*` 参数与状态面板） | 需要共享真实桌面的设备 |

## 快速开始

### 下载

[GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases) 为每个架构提供两种 musl 静态二进制。以下载 x86_64 CLI 为例：

```bash
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64
chmod +x shell-remote-x86_64
```

其余架构（`aarch64`、`armv7`）与桌面版（`shell-remote-ui-<arch>`）见 [docs/install.md](docs/install.md)。

### 编译

```bash
git clone https://github.com/zzttzzmyswy/shell-remote.git
cd shell-remote
cargo build --release --no-default-features
```

`--no-default-features` 产出 lean CLI（`target/release/shell-remote`，不编译任何桌面依赖）。需要桌面共享时改为 `cargo build --release`，即同时产出 `shell-remote` 与 `shell-remote-ui`。交叉编译与静态 musl 链路见 [docs/build.md](docs/build.md)。

### 启动 Relay

```bash
./shell-remote relay --auth YourStrongPassword --bind 0.0.0.0:3000
```

`--auth` 必填。relay 默认在监听端口直接提供 HTTPS（首次启动自动生成并持久化自签证书）；加 `--no-tls` 改为纯 HTTP。全部参数见 [docs/install.md](docs/install.md)。

### 启动 Agent

```bash
./shell-remote agent --relay-url https://relay.example.com --relay-insecure
```

`--relay-insecure` 用于信任 relay 的自签证书。连接固定 key 的会话：

```bash
./shell-remote agent --relay-url https://relay.example.com --relay-insecure --key YourFixedKey
```

需要桌面共享时把命令换成 `./shell-remote-ui agent`，并补 `--desktop-capture` 等参数（见 [docs/desktop.md](docs/desktop.md)）。开机自启的 systemd 部署见 [deploy/systemd-agent/](deploy/systemd-agent/)。

### 浏览器访问

打开 `https://<relay-ip>:3000`（自签证书需在浏览器确认一次），输入服务器密码与 Token。主区域是 xterm.js 终端，右侧是文件管理器，工具栏另有"桌面"与"网页"入口。

## 文档索引

| 文档 | 内容 |
|------|------|
| [docs/install.md](docs/install.md) | 各架构下载、relay / agent 全部参数、systemd 部署 |
| [docs/build.md](docs/build.md) | 编译、musl 静态链路、四平台交叉编译与打包脚本 |
| [docs/desktop.md](docs/desktop.md) | 桌面共享、编码与捕获后端、LAN / P2P 直连、状态面板 |
| [docs/web-map.md](docs/web-map.md) | 网页映射 v2 / v1、能力协商、部署要求、安全与限制 |
| [docs/windows.md](docs/windows.md) | Windows agent 安装、启用方式、功能差异 |
| [docs/admin.md](docs/admin.md) | 管理后台、HTTP 端点清单、带宽限制 |
| [docs/upgrade.md](docs/upgrade.md) | 制品库、升级任务决策、下发字段、原子换机流程 |
| [docs/mcp.md](docs/mcp.md) | MCP 接入配置、协议流程、工具参数、登录环境、文件传输、命令审计 |
| [docs/sessions.md](docs/sessions.md) | 会话录制、Token 权限模型、文件管理器 |
| [docs/performance.md](docs/performance.md) | 性能隔离、弱网与鲁棒性 |
| [docs/weak-network-robustness.md](docs/weak-network-robustness.md) | 弱网故障矩阵与逐条修复记录（v0.54.0 基线） |
| [docs/history/alignment-ledger.md](docs/history/alignment-ledger.md) | rustdesk 对齐台账（历史归档） |

## 测试

```bash
cargo test --no-default-features
```

全特性构建（含桌面编码 / 捕获单测，X11 采集测试需 `Xvfb :99`）：

```bash
cargo test
```

浏览器端到端与压测脚本在 `tools/verify/`（playwright）与 `tools/bench/`，用法见各脚本头部注释。

## 许可证

MIT
