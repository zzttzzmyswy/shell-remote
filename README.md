# ssh-remote

自托管、轻量级的远程服务器协作工具。部署单个 Rust 二进制文件，即可通过浏览器共享终端会话、管理远程文件，并为 AI Agent 提供 MCP 协议接口。

## 核心功能

- **协同终端** — 多人通过浏览器同时查看和操作同一个 Shell 会话（基于 xterm.js + WebGL 加速渲染）
- **文件管理器** — 侧栏文件树，支持浏览、读取、写入、删除、重命名远程文件
- **MCP 服务器** — AI Agent（Claude 等）通过标准 MCP 协议在远程机器上执行命令、管理文件
- **单一二进制** — Web 静态资源嵌入编译产物，零外部文件依赖
- **Token 鉴权** — 临时 Token（默认）或固定密钥；支持读写和只读两种权限
- **沙箱隔离** — Agent 端文件操作限制在指定根目录内

## 架构

```
浏览器 (xterm.js + 文件UI)
        │ WebSocket
        ▼
┌───────────────┐          WebSocket          ┌──────────────┐
│   Relay       │ ◄─────────────────────────► │   Agent      │
│   路由 + 鉴权  │                              │   Shell + FS │
│   静态资源 +   │                              │   (目标机器)  │
│   MCP 端点    │                              └──────────────┘
└───────────────┘
        ▲
        │ MCP (HTTP SSE + JSON-RPC)
        │
  AI Agent (Claude 等)
```

- **Relay**：无状态消息路由，连接各方并执行权限检查，嵌入 Web 前端
- **Agent**：在目标机器上运行，管理 PTY Shell 和文件系统，通过 WebSocket 连接 Relay

## 快速开始

### 编译

```bash
# 需要 Rust 1.75+
git clone <repo-url> && cd ssh-remote
cargo build --release
```

产物为单个静态二进制文件 `target/release/ssh-remote`，可直接拷贝到任意 Linux/macOS 机器运行。

#### 静态链接编译（跨架构分发）

生成无任何运行时依赖（甚至无 libc）的二进制，适用于裸部署：

```bash
# 安装 musl target
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl

# 编译 amd64 静态二进制
cargo build --release --target x86_64-unknown-linux-musl

# 编译 arm64 静态二进制（需要 lld 链接器）
cargo build --release --target aarch64-unknown-linux-musl

# 验证静态链接
ldd target/x86_64-unknown-linux-musl/release/ssh-remote
# 输出: statically linked
```

产物可直接 `scp` 到任意同架构 Linux 机器运行，无需安装任何依赖。

### Docker 部署

```bash
# 构建镜像
docker build -t ssh-remote .

# 启动 Relay（中转服务器）
docker run -d --name ssh-remote-relay -p 3000:3000 ssh-remote relay --dev --bind 0.0.0.0:3000

# 查看 Agent Token（Relay 容器内运行 Agent）
docker exec ssh-remote-relay ssh-remote agent --relay-url ws://localhost:3000/ws
```

如果需要将 Agent 部署到独立容器或不同机器：

```bash
# 在目标机器上运行 Agent 容器
docker run -d --name ssh-remote-agent \
  --pid=host --network=host \
  ssh-remote agent \
  --relay-url ws://<relay-ip>:3000/ws \
  --root /host
```

选项说明：
- `--pid=host`：Agent 需要访问宿主机的进程命名空间以创建 PTY
- `--network=host`：Agent 通过 Host 网络直接连接 Relay（生产环境建议用 Docker 网络）
- `--root /host`：若需操作宿主机文件，挂载 `-v /:/host:rw`

### 启动 Relay（中转服务器）

在最外层可访问的机器上：

```bash
./ssh-remote relay --dev --bind 0.0.0.0:3000
```

输出：
```
Relay server listening on 0.0.0.0:3000 (dev mode, plaintext)
```

- `--dev`：开发模式，使用明文 WebSocket（生产环境应配置 TLS 证书）
- `--bind`：监听地址，默认 `0.0.0.0:3000`
- `--tls-cert` / `--tls-key`：TLS 证书和私钥路径（不使用 --dev 时必需）

### 启动 Agent（目标机器）

在需要远程操作的机器上：

```bash
./ssh-remote agent --relay-url ws://<relay-ip>:3000/ws --root /home/user
```

输出：
```
session_id: a1b2c3d4-...
  readwrite: e83f2a1b...
```

- `--relay-url`：Relay 的 WebSocket 地址
- `--key`：固定密钥（可选，不指定则使用随机临时 Token）
- `--root`：文件系统沙箱根目录（默认 `$HOME`）
- `--token-type`：Token 类型，`rw`（默认）、`ro`（只读）、`both`（生成 rw + ro 两个 Token）

### 浏览器访问

打开 `http://<relay-ip>:3000/?token=<agent输出的token>`，输入 Token 后点击 **Connect (Read-Write)** 或 **Connect (Read-Only)** 即可进入会话。

- 主区域：xterm.js 终端，可直接输入命令
- 右侧抽屉：文件管理器，双击文件可在编辑器中打开，右键菜单支持重命名和删除

多个用户使用相同 Token 可同时加入同一个会话，共享实时终端输出。

## AI Agent 接入 (MCP)

Relay 同时暴露 MCP 协议端点，AI 可通过以下地址接入：

- SSE 端点：`http://<relay-ip>:3000/mcp/sse?token=<token>`
- 消息端点：`http://<relay-ip>:3000/mcp/messages?token=<token>`

鉴权 Token 与浏览器相同。

### MCP 工具列表

所有工具名均以 `remote_` 前缀区分，避免与 AI Agent 自身工具混淆。

| 工具 | 参数 | 说明 |
|------|------|------|
| `exec_remote` | `cmd`, `timeout_ms?` | 在**远程目标机器**上执行一次性 Shell 命令（30s 超时） |
| `exec_remote_start` | `cmd` | 启动交互式命令会话，返回 `exec_id` 和初始输出。进程在后台运行 |
| `exec_remote_input` | `exec_id`, `data` | 向运行中的会话发送 stdin 输入，返回累计输出 |
| `exec_remote_close` | `exec_id` | 关闭会话（如果进程仍在运行则 kill），返回最终输出和退出码 |
| `exec_remote_list` | 无 | 列出当前所有活跃的执行会话及其状态 |
| `file_remote_read` | `path` | 读取**远程目标机器**上的文件（base64 编码） |
| `file_remote_write` | `path`, `content` | 写入内容到**远程目标机器**的文件 |
| `file_remote_list` | `path` | 列出**远程目标机器**上目录的内容 |

### 交互式命令示例

```
AI: exec_remote_start("sudo systemctl restart nginx")
    → exec_id: "abc123", stdout: "[sudo] password for root:"

AI: exec_remote_input("abc123", "mypassword\n")
    → exec_id: "abc123", stdout: "[sudo] password for root:\n", status: "running"

AI: 稍等片刻...

AI: exec_remote_close("abc123")
    → exec_id: "abc123", stdout: "[sudo] password for root:\n...done", status: "exited", exit_code: 0
```

## CLI 参考

```
ssh-remote relay --dev --bind 0.0.0.0:3000
       --bind <ADDR>         监听地址（默认 0.0.0.0:3000）
       --tls-cert <PATH>     TLS 证书路径
       --tls-key <PATH>      TLS 私钥路径
       --dev                 开发模式，跳过 TLS（明文）

ssh-remote agent --relay-url ws://relay:3000/ws
       --relay-url <URL>     Relay WebSocket 地址
       --key <KEY>           固定鉴权密钥
       --root <PATH>         文件系统沙箱根目录（默认 $HOME）
       --token-type <TYPE>   rw / ro / both（默认 rw）
```

## Token 权限模型

| Token 类型 | 终端输入 | 文件读写 | MCP 执行 |
|-----------|---------|---------|----------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly  | ❌ | 只读（list/read） | ❌ |

- **临时 Token**：64 位随机十六进制字符串，Agent 断连即失效
- **固定密钥**：通过 `--key` 指定，持久有效，Agent 重新连接后仍可使用

## 技术栈

| 层 | 技术 |
|----|------|
| 运行时 | Rust + Tokio 异步 |
| HTTP/WS 框架 | Axum |
| 终端 | portable-pty + xterm.js (WebGL) |
| 静态嵌入 | rust-embed |
| 前端 | 原生 HTML/CSS/JS（无框架、零构建） |
| MCP 协议 | HTTP SSE + JSON-RPC |

## 测试

```bash
cargo test
# 51 passed; 0 failed
```

## 安全注意事项

- 生产环境请使用 TLS（提供 `--tls-cert` 和 `--tls-key`），不要使用 `--dev`
- Token 仅在 Relay 内存中存储，不会写入磁盘
- Relay 不记录终端输出或文件内容
- Agent 文件操作限制在 `--root` 指定目录内，`resolve_path` 防 `../` 逃逸
- 只读 Token 的所有写操作在 Relay 层被拦截

## 许可证

MIT
