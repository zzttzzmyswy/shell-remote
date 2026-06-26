# shell-remote

[简体中文](README.md) | English

Self-hosted, lightweight remote server collaboration tool. Deploy a single Rust binary to share terminal sessions via browser, manage remote files, and expose MCP protocol endpoints for AI agents.

## Features

- **Collaborative Terminal** — Multiple users view and interact with the same shell session simultaneously (xterm.js + WebGL)
- **Multi-Tab Shells** — Each user independently switches between multiple PTY shells; tab changes never affect others
- **File Manager** — Side panel with breadcrumb navigation, upload, download, delete, rename, mkdir, refresh
- **MCP Server** — AI agents (Claude, etc.) execute commands on remote machines via standard MCP SSE Transport
- **SSE+POST Transport** — Full-stack HTTP SSE push + POST send; no WebSocket dependency, works behind any proxy
- **Single Binary** — All web assets embedded via `rust-embed`; zero external file dependencies
- **Token Authentication** — Random temporary tokens or fixed keys; read-write and read-only permission levels
- **Server Password** — Relay-level access password (`--auth`), required

## Architecture

```
Browser (xterm.js + File UI)
         │ SSE + POST /agent/session/sse + /agent/session/send
         ▼
┌───────────────┐   HTTP SSE+POST (/agent/events + /agent/send)   ┌──────────────┐
│   Relay       │ ◄─────────────────────────────────────────────► │   Agent      │
│   Route + Auth │                                                  │   Shell + FS │
│   Static + MCP│                                                  │   (target)   │
└───────────────┘                                                  └──────────────┘
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
# --auth required; TLS is terminated by a fronting reverse proxy (nginx/caddy)
./shell-remote relay --auth YourStrongPassword --bind 0.0.0.0:3000
```

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `0.0.0.0:3000` | Listen address |
| `--auth` | none | Server password (required) |

### Start Agent

```bash
# HTTP SSE+POST mode (works behind any reverse proxy)
./shell-remote agent --relay-url https://<relay-ip>
```

| Flag | Default | Description |
|------|---------|-------------|
| `--relay-url` | `https://localhost:3000` | Relay URL (SSE+POST protocol) |
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

## Windows Agent

Requirements: Windows 10 1809+ (ConPTY minimum).

Install: download `shell-remote-x86_64.exe` from releases, rename to `shell-remote.exe`, or `cargo install --path .`.

Start:

```powershell
# default cmd
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx

# using PowerShell
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx --shell powershell.exe
```

Notes: run as administrator for full filesystem access; interactive programs (ssh/vim) are not supported in the MCP exec path; file upload (upload) is not supported on the Windows agent — use `fs:write` (base64) instead; file download (read) works.

### Cross-compile from Linux

```bash
rustup target add x86_64-pc-windows-gnu
# requires x86_64-w64-mingw32-gcc (mingw-w64)
cargo build --release --target x86_64-pc-windows-gnu
```

### Feature comparison

| Feature | Linux/macOS | Windows |
|---------|-------------|---------|
| PTY interactive shell | ✅ | ✅ (ConPTY) |
| Command execution | ✅ | ✅ (cmd / pwsh) |
| File browse/read/write/rename/delete | ✅ | ✅ |
| File download (read) | ✅ | ✅ |
| File upload (upload) | ✅ | ❌ (use fs:write) |
| Interactive programs (ssh/vim) | ✅ | ⚠️ not supported in exec path |
| File permission bits (mode) | ✅ | placeholder (no POSIX perms) |

## API Endpoints

| Path | Method | Description |
|------|--------|-------------|
| `/agent/session/sse` | GET → SSE | Browser receive stream |
| `/agent/session/send` | POST | Browser send messages |
| `/agent/events` | GET → SSE | Agent receive stream |
| `/agent/send` | POST | Agent send messages |
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

### Tool: shell_remote

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `token` | string | Yes | shell_remote token (agent session token) |
| `cmd` | string | Yes | Shell command to execute |
| `timeout_ms` | number | No | Timeout in milliseconds (default 30000, max 300000) |

Example call:

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
| HTTP | Axum |
| Terminal | portable-pty + xterm.js |
| Embedding | rust-embed |
| Frontend | Vanilla HTML/CSS/JS |
| MCP | SSE Transport + JSON-RPC |

## Tests

```bash
cargo test
# 119 passed; 0 failed (including integration test)
```

## License

MIT
