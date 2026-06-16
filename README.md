# shell-remote

English | [简体中文](README_zh.md)

Self-hosted, lightweight remote server collaboration tool. Deploy a single Rust binary to share terminal sessions via browser, manage remote files, and expose MCP protocol endpoints for AI agents.

## Features

- **Collaborative Terminal** — Multiple users view and interact with the same shell session simultaneously (xterm.js + WebGL rendering)
- **Multi-Tab Shells** — Each user independently switches between multiple PTY shells; one user's tab switch never affects others
- **File Manager** — Side panel with breadcrumb navigation, upload (HTTP streaming, no file size limit), download, delete, rename, mkdir, and refresh
- **MCP Server** — AI agents (Claude, etc.) execute commands and manage files on remote machines via standard MCP protocol
- **Dual Transport** — Agent connects via WebSocket (`ws://`) or HTTP SSE+POST (`https://`) — fully HTTP/1.1/2/3 compatible
- **Single Binary** — All web assets embedded via `rust-embed`; zero external file dependencies
- **Token Authentication** — Random temporary tokens or fixed keys; read-write and read-only permission levels
- **Server Password** — Optional relay-level access password (`--auth`) to protect the web UI

## Architecture

```
Browser (xterm.js + File UI)
        │ WebSocket /agent
        ▼
┌───────────────┐     WS (/agent) or HTTP (SSE+POST)    ┌──────────────┐
│   Relay       │ ◄───────────────────────────────────► │   Agent      │
│   Route + Auth │                                        │   Shell + FS │
│   Static + MCP│                                        │   (target)   │
└───────────────┘                                        └──────────────┘
        ▲
        │ MCP (/agent/mcp/sse + /agent/mcp/messages)
        │
  AI Agent (Claude, etc.)
```

- **Relay**: Stateless message router that connects all parties and enforces permissions; embeds the web frontend
- **Agent**: Runs on the target machine, manages PTY shells and filesystem; supports both WebSocket and HTTP SSE+POST transport

## Quick Start

### Build

```bash
# Requires Rust 1.75+
git clone https://github.com/zzttzzmyswy/shell-remote.git && cd shell-remote
cargo build --release
```

Produces a single static binary at `target/release/shell-remote`.

#### Static Linking (cross-platform distribution)

See [BUILD.md](BUILD.md) for detailed cross-compilation instructions (x86_64, aarch64, armv7).

### Download Pre-built

Pre-built musl-static binaries for three architectures are available on [GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases).

```bash
# x86_64 (Intel/AMD)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64 && chmod +x shell-remote-x86_64

# aarch64 (ARM 64-bit, Raspberry Pi 4/5)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-aarch64 && chmod +x shell-remote-aarch64

# armv7 (ARM 32-bit, Raspberry Pi 2/3)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-armv7 && chmod +x shell-remote-armv7
```

Or visit `/download` on any running relay for a download page with platform detection and copyable curl commands.

### Docker

```bash
docker build -t shell-remote .
docker run -d --name shell-remote-relay -p 3000:3000 shell-remote relay --dev --bind 0.0.0.0:3000
```

For agent on target machine:

```bash
docker run -d --name shell-remote-agent \
  --pid=host --network=host \
  shell-remote agent \
  --relay-url ws://<relay-ip>:3000 \
  --root /host
```

### Start Relay

```bash
./shell-remote relay --dev --bind 0.0.0.0:3000
```

Options:

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `0.0.0.0:3000` | Listen address |
| `--dev` | false | Development mode (plaintext WebSocket) |
| `--auth` | `password` | Server access password for the web UI |
| `--bin-dir` | — | Path to directory with pre-built binaries (for /download page) |
| `--tls-cert` | — | TLS certificate path |
| `--tls-key` | — | TLS private key path |

### Start Agent

```bash
# WebSocket mode (real-time, best latency)
./shell-remote agent --relay-url ws://<relay-ip>:3000 --root /home/user

# HTTP SSE+POST mode (HTTP/2/3 compatible, works behind any reverse proxy)
./shell-remote agent --relay-url https://<relay-ip> --root /home/user
```

Options:

| Flag | Default | Description |
|------|---------|-------------|
| `--relay-url` | `ws://localhost:3000` | Relay URL. `ws://`=WebSocket, `https://`=SSE+POST |
| `--key` | — | Fixed auth key (random if omitted) |
| `--root` | `$HOME` | Root directory for file browser |
| `--token-type` | `rw` | `rw`, `ro`, or `both` |
| `--shell` | `/bin/bash` | Shell binary path |

Output:

```
session: a1b2c3d4
  rw: e83f2a1b9c...
```

- `session:` — 8-char session ID (for logging only)
- `rw:` / `ro:` — authentication tokens (use these in the browser)

### Browser Access

Open `http://<relay-ip>:3000`, enter the server password (if configured) and session token, then click **Read-Write** or **Read-Only**.

- Main area: xterm.js terminal with WebGL acceleration
- Right drawer: file manager with breadcrumb navigation, upload/download, and context menu

Multiple users with the same token join the same session and share real-time terminal output. Each user independently switches tabs without affecting others.

## API Endpoints

All client-facing endpoints are under `/agent`:

| Path | Method | Description |
|------|--------|-------------|
| `/agent` | GET → WS | WebSocket for browser and agent (WS mode) |
| `/agent/events` | GET → SSE | Agent receive stream (HTTP mode) |
| `/agent/send` | POST | Agent send messages (HTTP mode) |
| `/agent/upload` | POST | File upload (Bearer token auth) |
| `/agent/mcp/sse` | GET → SSE | MCP SSE endpoint |
| `/agent/mcp/messages` | POST | MCP JSON-RPC messages |

## AI Agent Integration (MCP)

Relay exposes MCP protocol endpoints:

- SSE: `http://<relay-ip>:3000/agent/mcp/sse?token=<token>`
- Messages: `http://<relay-ip>:3000/agent/mcp/messages` (token via `Authorization: Bearer <token>` header, query parameter `?token=` as fallback)

### MCP Tools

All tools prefixed with `remote_` to avoid conflicts with the AI agent's own tools.

| Tool | Parameters | Description |
|------|-----------|-------------|
| `exec_remote` | `cmd`, `timeout_ms?` | Execute a one-shot shell command (30s timeout) |
| `exec_remote_start` | `cmd` | Start interactive command session, returns `exec_id` and initial output |
| `exec_remote_input` | `exec_id`, `data` | Send stdin to a running session |
| `exec_remote_close` | `exec_id` | Close session (kill if running), returns final output and exit code |
| `exec_remote_list` | — | List all active execution sessions |
| `file_remote_read` | `path` | Read a file (base64-encoded) |
| `file_remote_write` | `path`, `content` | Write content to a file |
| `file_remote_list` | `path` | List directory contents |

## Token Permission Model

| Token Type | Terminal Input | File Operations | MCP Execution |
|-----------|---------------|-----------------|---------------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly | ❌ | list/read only | ❌ |

- **Temporary Token**: 64-char random hex string; invalidated when agent disconnects
- **Fixed Key**: Set via `--key`; persists across agent reconnections

## File Manager

- Breadcrumb path navigation with clickable segments
- Upload via HTTP POST to `/agent/upload` (`Authorization: Bearer <token>` header), streaming body (no file size limit)
- Download via WebSocket with `_mcp_request_id` routing
- Delete, rename, mkdir, refresh
- Drag-to-resize side panel
- Real-time upload progress via XHR `upload.onprogress`

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Runtime | Rust + Tokio async |
| HTTP/WS | Axum |
| Terminal | portable-pty + xterm.js (WebGL) |
| Static Embedding | rust-embed |
| Frontend | Vanilla HTML/CSS/JS (zero build step) |
| Fonts | CaskaydiaCove Nerd Font Mono + Sarasa Term SC |
| MCP | HTTP SSE + JSON-RPC |

## Tests

```bash
cargo test
# 100 passed; 0 failed
```

## License

MIT
