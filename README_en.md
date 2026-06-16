# shell-remote

[简体中文](README.md) | English

Self-hosted, lightweight remote server collaboration tool. Deploy a single Rust binary to share terminal sessions via browser, manage remote files, and expose MCP protocol endpoints for AI agents.

## Features

- **Collaborative Terminal** — Multiple users view and interact with the same shell session simultaneously (xterm.js + WebGL)
- **Multi-Tab Shells** — Each user independently switches between multiple PTY shells; tab changes never affect others
- **File Manager** — Side panel with breadcrumb navigation, upload, download, delete, rename, mkdir, refresh
- **MCP Server** — AI agents (Claude, etc.) execute commands on remote machines via standard MCP SSE Transport
- **Dual Transport** — Agent connects via WebSocket (`ws://`) or HTTP SSE+POST (`https://`) — fully HTTP/1.1/2/3 compatible
- **Single Binary** — All web assets embedded via `rust-embed`; zero external file dependencies
- **Token Authentication** — Random temporary tokens or fixed keys; read-write and read-only permission levels
- **Server Password** — Relay-level access password (`--auth`), required unless `--dev` mode

## Architecture

```
Browser (xterm.js + File UI)
        │ WebSocket /agent
        ▼
┌───────────────┐     WS or HTTP (SSE+POST)    ┌──────────────┐
│   Relay       │ ◄───────────────────────────► │   Agent      │
│   Route + Auth │                               │   Shell + FS │
│   Static + MCP│                               │   (target)   │
└───────────────┘                               └──────────────┘
        ▲
        │ MCP (/agent/mcp/sse + /agent/mcp/messages)
        │
  AI Agent (Claude, etc.)
```

## Quick Start

### Download

```bash
# x86_64 (Intel/AMD)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64 && chmod +x shell-remote-x86_64

# aarch64 (ARM 64-bit, Raspberry Pi 4/5, cloud)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-aarch64 && chmod +x shell-remote-aarch64

# armv7 (ARM 32-bit, Raspberry Pi 2/3)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-armv7 && chmod +x shell-remote-armv7
```

### Build

```bash
git clone https://github.com/zzttzzmyswy/shell-remote.git && cd shell-remote
cargo build --release
```

### Start Relay

```bash
# Development mode
./shell-remote relay --dev --auth password --bind 0.0.0.0:3000

# Production mode (--auth required)
./shell-remote relay --auth YourStrongPassword --bind 0.0.0.0:3000
```

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `0.0.0.0:3000` | Listen address |
| `--dev` | false | Development mode (no TLS, allows no --auth) |
| `--auth` | none | Server password (required unless --dev) |
| `--bin-dir` | — | Path to pre-built binaries |
| `--tls-cert` | — | TLS certificate path |
| `--tls-key` | — | TLS private key path |

### Start Agent

```bash
# WebSocket mode (real-time, lowest latency)
./shell-remote agent --relay-url ws://<relay-ip>:3000

# HTTP SSE+POST mode (works behind any reverse proxy)
./shell-remote agent --relay-url https://<relay-ip>
```

| Flag | Default | Description |
|------|---------|-------------|
| `--relay-url` | `ws://localhost:3000` | Relay URL. `ws://`=WebSocket, `https://`=SSE+POST |
| `--key` | — | Fixed auth key (random if omitted) |
| `--root` | `$HOME` | File manager default directory |
| `--token-type` | `rw` | Token type: `rw`, `ro`, or `both` |
| `--shell` | `/bin/bash` | Shell binary path |

Output:

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

### Browser Access

Open `http://<relay-ip>:3000`, enter server password and token. Main area: xterm.js terminal. Right drawer: file manager.

## API Endpoints

| Path | Method | Description |
|------|--------|-------------|
| `/agent` | GET → WS | WebSocket for browser and agent |
| `/agent/events` | GET → SSE | Agent receive stream (HTTP mode) |
| `/agent/send` | POST | Agent send messages (HTTP mode) |
| `/agent/upload` | POST | File upload |
| `/agent/mcp/sse` | GET → SSE | MCP SSE Transport endpoint |
| `/agent/mcp/messages` | POST | MCP JSON-RPC messages |

## AI Agent Integration (MCP)

### Configuration

```json
{
  "transport": "sse",
  "url": "https://<relay-host>/agent/mcp/sse",
  "headers": { "X-Auth": "your-server-password" },
  "timeout": 60,
  "sse_read_timeout": 300
}
```

Protocol flow: `GET /sse` → `endpoint` event → `POST /messages` → `202 Accepted` → SSE `message` response.

### Tool: exec_remote

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `token` | string | Yes | Agent session token |
| `cmd` | string | Yes | Shell command to execute |
| `timeout_ms` | number | No | Timeout in milliseconds (default 30000, max 300000) |

Example call:

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

Token is passed in arguments, not in URL or headers. Commands execute via `sh -c`.

## Token Permissions

| Token Type | Terminal Input | File Ops | MCP Exec |
|-----------|---------------|----------|----------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly | ❌ | list/read | ❌ |

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Runtime | Rust + Tokio async |
| HTTP/WS | Axum |
| Terminal | portable-pty + xterm.js |
| Embedding | rust-embed |
| Frontend | Vanilla HTML/CSS/JS |
| MCP | SSE Transport + JSON-RPC |

## Tests

```bash
cargo test
# 104 passed; 0 failed (including integration test)
```

## License

MIT
