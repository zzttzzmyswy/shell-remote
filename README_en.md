# shell-remote

[简体中文](README.md) | English

Self-hosted, lightweight remote server collaboration tool. Deploy a single Rust binary to share terminal sessions via browser, manage remote files, and expose MCP protocol endpoints for AI agents.

## Features

- **Collaborative Terminal** — Multiple users view and interact with the same shell session simultaneously (xterm.js + WebGL)
- **Multi-Tab Shells** — Each user independently switches between multiple PTY shells; tab changes never affect others
- **File Manager** — Side panel with breadcrumb navigation, upload, download, delete, rename, mkdir, refresh
- **MCP Server** — AI agents (Claude, etc.) execute commands on remote machines via standard MCP SSE Transport
- **Tabbed Admin Panel** — The admin page is organized into six tabs: Overview / Sessions / Devices / Recordings / Access log / Settings; the active tab is remembered
- **Device Management** — The Devices tab lists every agent with its probed host info (CPU model, architecture, OS, kernel, hostname, agent version) and supports keyword / architecture / status filtering with one-click connect
- **Atomic Agent Self-Upgrade** — One-click per-device upgrade from the Devices tab: the agent downloads the new binary from the relay, verifies its SHA-256, smoke-tests executability, atomically replaces itself and restarts with its original arguments; progress is shown live in the device row and any failure keeps the old binary with a clear error
- **Connection Self-Healing** — A registered-but-unreachable device no longer leaves the browser on a blank terminal: the session page shows "agent disconnected, reconnecting" and auto-reconnects; the terminal resumes once the agent link is back. If a join cannot be delivered because the agent channel is stale (reconnect/eviction window), the relay tells the browser to re-join, and the browser-side join-ack watchdog reconnects after 8s without an agent reply — no silent failure can leave a permanently empty terminal
- **Desktop Sharing** — Agent captures the real X11/Windows desktop, encodes H.264 (openh264 software encoder) with adaptive bitrate 800–200 kbps, and streams fragmented MP4 to browsers over MSE; the session page switches freely between terminal and desktop views (desktop is off by default; click a button to open it)
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
| `--record-dir` | none | Directory to record terminal sessions (asciinema cast v2); unset disables |
| `--download-dir` | none | Offline binary distribution directory: place per-platform agent binaries by file name (e.g. `shell-remote-x86_64`, `shell-remote-aarch64`, `shell-remote-armv7`, `shell-remote-x86_64.exe`), served at `/download/<name>`. The install scripts download from this relay **first**, falling back to GitHub mirrors — for intranet / restricted networks |
| `--agent-upgrade-dir` | none | Directory with staged agent upgrade artifacts (`shell-remote-<arch>[.exe]`, optional `shell-remote-<arch>.version` companion); unset disables the upgrade button |

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
| `--session-id` | — | Custom session id (5-20 alphanumeric) shown in admin to distinguish devices; **reusable** — a new agent registering with the same id takes over the old session (old tokens invalidated), no more conflict error |
| `--desktop-capture` | `auto` | Desktop capture backend: `auto` / `x11` / `wayland` / `gdi` / `none` (`none` disables desktop; native Wayland capture needs xdg-desktop-portal, not yet implemented — use X11/XWayland) |
| `--desktop-codec` | `h264` | Desktop video codec (H.264 only for now; see "Desktop Sharing") |
| `--desktop-fps` | `15` | Desktop capture frame rate |
| `--desktop-max-bitrate` | `800` | Maximum encode bitrate (kbps, 800 as specified) |
| `--desktop-min-bitrate` | `200` | Minimum encode bitrate (kbps, 200 as specified) |
| `--desktop-display` | `$DISPLAY` | Selected X11 display (e.g. `:1`), defaults to `$DISPLAY` |

Output:

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

### Browser Access

Open `http://<relay-ip>:3000`, enter server password and token. Main area: xterm.js terminal. Right drawer: file manager.

## Desktop Sharing

Share the agent's real desktop in agent mode: click the "Desktop" button in the browser session page to open the stream; the picture is H.264-encoded in real time and forwarded over the relay (`/agent/desktop/stream`).

- Start: click the "Desktop" toolbar button in the session page (off by default, click to open) → on `desktop:started`, the browser auto-connects the video stream; click "Terminal" to switch back.
- Permission: starting/stopping the stream needs an rw token (`requires_write`); watching the stream works with rw or ro.
- Bitrate: adaptive, **max 800 kbps, min 200 kbps** (`--desktop-max-bitrate` / `--desktop-min-bitrate`, default 800/200).
- Encoding: software (openh264, BSD license) with `--desktop-codec h264`. **Hardware encoding (VAAPI / Windows Media Foundation) is a planned extension — current version is pure software.**
- Capture: X11 (incl. XWayland) and Windows GDI are implemented; native Wayland needs xdg-desktop-portal + PipeWire, not included here (the agent prints a clear error; use XWayland).
- Transport: fragmented MP4 streamed to browser MSE; late joiners start at the most recent key frame (IDR).
- Other codecs (VP8/VP9/HEVC): omitted to keep the binary small and browsers compatible (x265/libvpx are large and HEVC MSE support is spotty). H.264 has the widest MSE support, matching the requirement "if encoders are too large, ship only the format that compresses well and every browser can decode".

```bash
./shell-remote agent --relay-url https://relay.example.com \
  --desktop-capture auto --desktop-fps 15 \
  --desktop-max-bitrate 800 --desktop-min-bitrate 200
```

## Windows Agent

Requirements: Windows 10 1809+ (ConPTY minimum).

One-line install & run (relay URL auto-injected):

```powershell
# default cmd
irm http://your-relay:3000/agent/install.ps1 | iex

# or manually download shell-remote-x86_64.exe from releases and rename to shell-remote.exe
```

Download to the current directory without running:

```powershell
& ([scriptblock]::Create((irm http://your-relay:3000/agent/install.ps1))) --download-only
```

> Linux/macOS equivalent: `curl -fsSL http://your-relay:3000/agent/install | sh` (run) or `... | sh -s -- --download-only` (download only). If curl is missing, use `wget -qO- http://your-relay:3000/agent/install | sh` (or `... | sh -s -- --download-only`). The script supports both curl and wget internally and tries multiple GitHub mirrors, so it works in most network environments.

Manual start:

```powershell
# default cmd
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx

# using PowerShell
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx --shell powershell.exe
```

Notes: run as administrator for full filesystem access; interactive programs (ssh/vim) are not supported in the MCP exec path; file download (read) works. Terminal output and MCP command results are automatically normalized from non-UTF-8 console encodings (e.g. GBK/cp936) to UTF-8, so browsers and AI agents never need to know the target machine's encoding.

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
| File upload (upload) | ✅ | ✅ |
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

## Admin Panel

The relay can optionally enable a web admin panel: view sessions/tokens, kick sessions, manage token permissions, view/rotate the server password, and see runtime status. Disabled by default; must be enabled via CLI. **The homepage has no link to it — you must type the secret sub-path manually.**

### Enable

```bash
shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 \
  --admin-path /your-secret-path --admin-pass ADMIN_PASSWORD
# --admin-user defaults to "admin"; omit --admin-path to leave the panel fully disabled
```

### Access

Open `http://<relay-ip>:3000/your-secret-path` (the value of `--admin-path`) in a browser and sign in with `--admin-user` / `--admin-pass`. The secret path is the first barrier; a successful login issues an HttpOnly + SameSite=Strict session cookie (12h TTL).

### Features

- **Overview**: version, uptime, agent total/online, browser total, per-session token list with permissions, connected browser count.
- **Session monitoring**: per-session online status and last-active time (refreshes every few seconds).
- **Access log (lightweight bastion audit)**: records browser/MCP connect & disconnect events (session, first 8 chars of token, permission, time), bounded to the latest 500 entries, visible in the panel.
- **Token management**: revoke a single token, regenerate a session's tokens (old ones invalidated), toggle token permission (rw↔ro).
- **Jump to terminal**: per-session "Connect" button opens that session's browser terminal in a new tab (token pre-filled; server password still typed manually).
- **Session recording**: with `--record-dir`, interactive terminal I/O (output + input) is written as asciinema cast v2; the panel shows recording status; files replay with `asciinema play`.
- **Recording management**: the "Recordings" panel lists all recorded files (terminal casts + MCP command audits) with their time and size; terminal casts replay in an in-panel player (xterm.js, with play/pause, 0.25x–4x speed, and seek bar), MCP command records are shown as structured cards, and any recording can be deleted.
- **Kick session**: disconnect that agent and all its browsers and invalidate its tokens.
- **Server password**: view the current `--auth`, rotate it live (takes effect immediately).
- **Chinese / English toggle**: switch the panel UI between zh and en (auto-detects browser language, remembered in localStorage).

### Security notes

- The admin page is not in the public static asset folder — it cannot be fetched via `/admin.html` or similar.
- Two layers: secret path (hidden entry) + user/password login.
- Admin sessions live in memory only; relay restart requires re-login.
- Known limitation: after revoking/regenerating a token, an agent that reconnects via `register_existing` (replaying its cached tokens) may re-introduce that token; does not affect the live session.

## Atomic Agent Self-Upgrade

Enable with `--agent-upgrade-dir <dir>` on the relay, then stage new agent binaries in that directory with the same naming as the release artifacts (see `scripts/build-releases.sh`): `shell-remote-x86_64`, `shell-remote-aarch64`, `shell-remote-armv7`, or `shell-remote-x86_64.exe` on Windows. An optional `shell-remote-<arch>.version` file (e.g. `0.19.0`) labels the target version in the UI.

Every online device in the admin **Devices** tab has an **Upgrade** button. Clicking it:

1. The relay picks the artifact matching the device's reported architecture, hashes it (SHA-256), and sends `agent:upgrade` to that agent.
2. The agent downloads the artifact from the relay (authenticated with its own read-write token), reporting progress percentages.
3. The SHA-256 is verified — a mismatch aborts and keeps the old binary.
4. On Unix the new binary is smoke-tested with `--version` to confirm it actually runs on this platform (guards against wrong-arch or truncated artifacts); Windows skips this (SHA-256 is the integrity gate).
5. The binary is atomically replaced in its own directory (atomic `rename` on Unix; on Windows, when the running `.exe` blocks replacement, a helper `.bat` defers the swap), then the agent re-launches itself with its original arguments and the old process exits.

Progress is shown live in the device row's version cell (started / downloading n% / verifying / installing / failure reason); once upgraded it shows a green "upgraded v<version>" and the version column reflects the new agent version.

Notes:

- The agent's binary directory must be writable (`rename` needs write permission on it); failures surface clearly in the panel.
- The restarted process re-registers with its fixed `--key`, keeping its identity; agents started with a temporary token (no `--key`) get freshly minted tokens after the restart, invalidating old ones.

## Session Recording

Add `--record-dir <dir>` to record interactive terminal sessions (MCP exec is not recorded):

```bash
shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 --record-dir /var/log/shell-remote
```

- Format: asciinema cast v2 (JSONL); replay with `asciinema play xxx.cast` or xterm.js.
- Records output + input streams; **the input stream includes sensitive keystrokes typed in the terminal (e.g. sudo passwords) — protect the record directory's filesystem permissions**.
- One file per session: `{session_id}_{unix_timestamp}.cast`; with agent `--session-id`, the filename is that id.
- Captured at the relay; no agent change. Kicked/idle-reaped sessions flush and close their files.

### MCP command audit

With `--record-dir` enabled, every MCP `shell_remote` tool call is also audited (same opt-in and directory as terminal recording), written to a per-session file `{session_id}_{unix_timestamp}.audit.jsonl`, one JSON object per line:

```json
{"ts":"2026-07-06T12:34:56Z","unix_ms":1783339200000,"session_id":"seSupportBot","token_prefix":"b4e2ec42","permission":"rw","cmd":"ls -la","timeout_ms":30000,"duration_ms":12,"status":"ok","exit_code":0,"stdout_len":1234,"stderr_len":0,"stdout":"...","stderr":"..."}
```

- `status`: `ok` / `timeout` / `disconnected` / `no_agent` / `rejected_readonly`.
- `token_prefix` stores only the first 8 chars of the token (enough to correlate with the admin token list without leaking the full secret).
- `stdout` / `stderr` are truncated to 4KB (`stdout_len` / `stderr_len` hold the full lengths).
- The admin overview shows a `●AUDIT` marker per session; kicked/idle-reaped sessions flush and close their audit file.
- The "Recordings" panel lists each audit file as an "MCP cmd" entry; clicking it shows a structured card (command, status, exit code, duration, permission, token prefix, stdout/stderr), and it can be deleted.
- **Audit files may also contain sensitive data in commands and output — protect the record directory's filesystem permissions.**

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

## Performance & congestion isolation

To prevent a large file transfer or a flood of terminal logs from making other sessions unresponsive, the relay isolates traffic at several layers:

- **Chunked file transfer**: uploads and downloads are streamed as ≤256KB base64 chunks. No single message is ever large enough to hold a worker thread with one giant synchronous encode or blow up memory.
- **Bounded channels**: the relay→agent and relay→browser SSE channels are bounded (256 entries). On overflow they drop loss-tolerant terminal-output frames first and keep control/result messages, so a stuck consumer can't grow memory without limit and starve the whole relay.
- **EventBuffer byte cap**: the per-session replay buffer is capped at 1000 entries *and* 8MB total, so large messages or a sustained log flood can't blow it up.
- **Backpressure**: upload chunks are sent with backpressure (await, not drop) when the agent falls behind; downloads stream from a dedicated task so they don't block the agent's terminal-input forwarding.

Trade-off: when a session's consumer is badly stuck, that session's transfer may fail or drop frames — but other sessions stay responsive.

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
# 173 passed; 0 failed (including integration test)
```

## License

MIT
