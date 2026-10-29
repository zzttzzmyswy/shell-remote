# shell-remote

[简体中文](README.md) | English

Self-hosted, lightweight remote collaboration tool: a single Rust binary that shares terminal sessions through a browser, manages remote files, shares the desktop, and exposes an MCP interface for AI agents. All web assets are embedded into the binary via `rust-embed`, so deployment needs no external files.

## Features

- **Collaborative terminal** — Several people view and drive the same PTY session in the browser (xterm.js + WebGL); each switches their own tabs independently
- **Weak-network resume** — PTYs survive relay reconnects: after a network drop, a relay restart or a half-open link, running commands continue and a re-join replays each tab's last 64KB of output. Both directions have idle timeouts and heartbeat-based dead-link detection. See [docs/performance.md](docs/performance.md)
- **Desktop sharing** — Captures the X11/Windows desktop and encodes it in pure software with AV1 (libaom) or H.264 (openh264) at an adaptive bitrate; the downlink picks LAN → WebRTC P2P → relay automatically. See [docs/desktop.md](docs/desktop.md)
- **Web mapping** — The "Web" button in the session page browses pages on the remote device's network (including services bound to the device's own `127.0.0.1`) through a relay/agent tunnel. v2 full mode runs scripts with SPA routing, cookie login, POST and WebSocket; v1 static mode renders static pages only. See [docs/web-map.md](docs/web-map.md)
- **MCP server** — AI agents run commands on the remote machine over standard MCP SSE Transport, with dedicated streaming endpoints for large files that keep bytes out of the LLM context. See [docs/mcp.md](docs/mcp.md)
- **File manager and recording** — Breadcrumb navigation, upload/download/rename/delete; asciinema cast v2 recording plus an MCP command audit trail. See [docs/sessions.md](docs/sessions.md)
- **Admin panel** — Six tabs (Overview / Sessions / Devices / Recordings / Access log / Settings) with device probing, one-click connect and one-click atomic self-upgrade. See [docs/admin.md](docs/admin.md)
- **SSE + POST transport** — HTTP push plus POST sends end to end; no WebSocket dependency, works behind any reverse proxy
- **Two binaries** — `shell-remote` (lean: terminal and MCP only) and `shell-remote-ui` (terminal + desktop) share the same relay and protocol and can join the same relay

## Architecture

```
Browser (xterm.js + file manager UI)
         │ SSE + POST /agent/session/sse + /agent/session/send
         ▼
┌───────────────┐   HTTP SSE+POST (/agent/events + /agent/send)   ┌──────────────┐
│   Relay       │ ◄─────────────────────────────────────────────► │   Agent      │
│  route + auth │                                                  │  shell + FS  │
│  static + MCP │                                                  │  (target)    │
└───────────────┘                                                  └──────────────┘
         ▲
         │ MCP (/agent/mcp/sse + /agent/mcp/messages)
         │
   AI agent (Claude, etc.)
```

- **Relay** — Routing hub; enforces authentication and permissions and embeds the web frontend and the MCP server
- **Agent** — Runs on the target machine and owns the PTYs and filesystem; desktop sharing additionally needs `shell-remote-ui`

| Binary | Subcommands | Capabilities | Typical use |
|--------|-------------|--------------|-------------|
| `shell-remote` | `relay` / `agent` | Terminal forwarding (PTY + file manager + MCP exec); the build contains no desktop code | Self-hosted relay; agents on NAS / cloud hosts / headless boxes; MCP integration |
| `shell-remote-ui` | `agent` | Terminal + desktop forwarding (all `--desktop-*` flags plus the status panel) | Devices whose real desktop must be shared |

## Quick Start

### Download

[GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases) publishes two musl-static binaries per architecture. Downloading the x86_64 CLI as an example:

```bash
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64
chmod +x shell-remote-x86_64
```

Other architectures (`aarch64`, `armv7`) and the desktop build (`shell-remote-ui-<arch>`) are covered in [docs/install.md](docs/install.md).

### Build

```bash
git clone https://github.com/zzttzzmyswy/shell-remote.git
cd shell-remote
cargo build --release --no-default-features
```

`--no-default-features` produces the lean CLI (`target/release/shell-remote`, no desktop dependency compiled). For desktop sharing use `cargo build --release`, which emits both `shell-remote` and `shell-remote-ui`. Cross-compilation and the static musl link are covered in [docs/build.md](docs/build.md).

### Start the relay

```bash
./shell-remote relay --auth YourStrongPassword --bind 0.0.0.0:3000
```

`--auth` is required. The relay serves HTTPS directly on the listen port, generating and persisting a self-signed certificate on first start; add `--no-tls` for plain HTTP. Every flag is documented in [docs/install.md](docs/install.md).

### Start an agent

```bash
./shell-remote agent --relay-url https://relay.example.com --relay-insecure
```

`--relay-insecure` trusts the relay's self-signed certificate. To join a session with a fixed key:

```bash
./shell-remote agent --relay-url https://relay.example.com --relay-insecure --key YourFixedKey
```

Swap the command for `./shell-remote-ui agent` and add `--desktop-capture` and friends to share a desktop (see [docs/desktop.md](docs/desktop.md)). For a systemd-managed agent at boot see [deploy/systemd-agent/](deploy/systemd-agent/).

### Browser access

Open `https://<relay-ip>:3000` (accept the self-signed certificate once) and enter the server password and token. The main area is the xterm.js terminal, the right drawer is the file manager, and the toolbar also offers the "Desktop" and "Web" views.

## Documentation

| Document | Contents |
|----------|----------|
| [docs/install.md](docs/install.md) | Per-architecture downloads, every relay / agent flag, systemd deployment |
| [docs/build.md](docs/build.md) | Building, the static musl link, four-platform cross-compilation and packaging scripts |
| [docs/desktop.md](docs/desktop.md) | Desktop sharing, capture and codec backends, LAN / P2P direct links, status panels |
| [docs/web-map.md](docs/web-map.md) | Web mapping v2 / v1, capability negotiation, deployment requirements, security and limits |
| [docs/windows.md](docs/windows.md) | Windows agent install, shell selection, feature differences |
| [docs/admin.md](docs/admin.md) | Admin panel, HTTP endpoint reference, bandwidth limiting |
| [docs/upgrade.md](docs/upgrade.md) | Artifact store, upgrade task decisions, payload fields, the atomic swap |
| [docs/mcp.md](docs/mcp.md) | MCP configuration, protocol flow, tool parameters, login environment, file transfer, command audit |
| [docs/sessions.md](docs/sessions.md) | Session recording, token permission model, file manager |
| [docs/performance.md](docs/performance.md) | Performance isolation, weak-network robustness |
| [docs/weak-network-robustness.md](docs/weak-network-robustness.md) | Weak-network failure matrix and per-item fix log (v0.54.0 baseline) |
| [docs/history/alignment-ledger.md](docs/history/alignment-ledger.md) | rustdesk alignment ledger (historical archive) |

## Tests

```bash
cargo test --no-default-features
```

Full-feature build (includes desktop encoder / capture unit tests; the X11 capture tests need `Xvfb :99`):

```bash
cargo test
```

Browser end-to-end and load scripts live in `tools/verify/` (playwright) and `tools/bench/`; usage is documented in each script's header.

## License

MIT
