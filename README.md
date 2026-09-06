# shell-remote

简体中文 | [English](README_en.md)

自托管、轻量级的远程服务器协作工具。单个 Rust 二进制文件，即可通过浏览器共享终端会话、管理远程文件，并为 AI Agent 暴露 MCP 协议接口。

## 功能

- **协同终端** — 多人通过浏览器同时查看和操作同一个 Shell 会话（xterm.js + WebGL 渲染）
- **多 Tab 独立** — 每位用户独立切换多个 PTY Shell 标签页，互不干扰
- **文件管理器** — 侧栏面板，面包屑导航、上传、下载、删除、重命名、新建文件夹、刷新
- **MCP 服务器** — AI Agent（Claude 等）通过标准 MCP SSE Transport 协议在远程机器上执行命令
- **管理后台多标签页** — 后台按 概览 / 会话 / 设备 / 录像 / 访问日志 / 设置 六个标签页组织，选中页自动记忆
- **设备管理** — 后台"设备"面板展示每台 agent 探测上报的主机信息（CPU 型号、系统架构、系统、内核、主机名、agent 版本），支持关键字 / 架构 / 在线状态筛选，逐台一键连接
- **Agent 原子自升级** — 后台"设备"面板逐台一键触发 agent 自升级：agent 从 relay 下载新二进制 → SHA-256 校验 → 可执行性冒烟测试 → 同目录原子替换 → 以原参数重启；全程进度实时显示在设备行上，任何一步失败都会保留原二进制并给出明确错误
- **连接自愈** — 设备已注册但 agent 链路未建立或断开时不再僵死在空白终端：终端页提示"设备连接中断，正在自动重连"并自动重连，agent 恢复后终端自动回放续上；若 join 因 agent 通道失效（重连/顶替窗口）无法送达，relay 会通知浏览器重连；浏览器侧另有 join 应答看门狗，8s 无应答即自动重连——任何链路静默丢失都不会留下永久的空白终端
- **桌面共享** — agent 捕获 X11/Windows 桌面，H.264（openh264 软编）实时编码，动态码率 800–200kbps 自适应，浏览器 MSE 播放；会话页可在"终端/桌面"间任意切换（桌面默认关闭，点击按钮开流）
- **P2P 直连（阶段1/2）** — 桌面流下行优先直连：同网段浏览器经 `--desktop-lan-port` 直连 agent 本地端点（LAN），否则 WebRTC DataChannel（WebRTC），均失败自动回退 relay 转发。三种路径用户无感切换，任何时刻不劣于纯 relay
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
| `--download-dir` | 无 | 离线二进制分发目录：目录内按文件名（如 `shell-remote-x86_64`、`shell-remote-aarch64`、`shell-remote-armv7`、`shell-remote-x86_64.exe`）放置各平台 agent 二进制，经 `/download/<文件名>` 对外提供；**安装脚本会优先从本 relay 下载**，GitHub 镜像仅作回退（适合内网/镜像受限环境） |
| `--agent-upgrade-dir` | 无 | agent 自升级制品目录（`shell-remote-<arch>[.exe]`，可选 `shell-remote-<arch>.version` 标注版本）；不设则"设备"页升级功能不可用 |

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
| `--session-id` | — | 自定义会话 ID（5-20 位字母数字），后台据此区分设备；**可重复使用**——新的 agent 用相同 ID 注册会顶替旧会话（旧 Token 失效），不再报冲突 |
| `--desktop-capture` | `auto` | 桌面捕获后端：`auto` / `x11` / `wayland` / `gdi` / `none`（`none` 关闭桌面功能；Wayland 需要 xdg-desktop-portal，暂未实现，建议 X11/XWayland） |
| `--desktop-codec` | `h264` | 桌面编码格式（当前仅 H.264；VP8/VP9/HEVC 因许可与浏览器兼容原因未内置，见下） |
| `--desktop-fps` | `15` | 桌面捕获帧率 |
| `--desktop-max-bitrate` | `800` | 最大编码码率（kbps，用户需求原数值 800） |
| `--desktop-min-bitrate` | `200` | 最小编码码率（kbps，用户需求原数值 200） |
| `--desktop-display` | `$DISPLAY` | 指定 X11 显示（如 `:1`），默认取 `$DISPLAY` |
| `--desktop-lan-port` | `0` | LAN 直连桌面流监听端口（阶段2）。`0` = 不启用；非 0 时同网段浏览器直接 `http://agent-ip:port/agent/desktop/stream` 拉流，绕开 relay |

输出示例：

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

- `session:` — 8 位会话 ID（仅用于日志）
- `rw:` / `ro:` — Token（浏览器登录或 MCP 调用使用）

### 浏览器访问

打开 `http://<relay-ip>:3000`，输入服务器密码及 Token 即可连接。主区域为 xterm.js 终端，右侧为文件管理器。

## 桌面共享

在 agent 模式下共享设备真实桌面：浏览器端"桌面"按钮开流，画面以 H.264 实时编码后经 relay 转发播放（`/agent/desktop/stream`）。

- 启动：浏览器会话页点击工具栏"桌面"按钮（默认关闭、点击才开流）→ 收到 `desktop:started` 后自动连接视频流；再点"终端"切回。
- 权限：开流/关流需 rw Token（`requires_write`）；观看桌面画面 rw/ro 均可。
- 码率：按用户需求 **最高 800kbps、最低 200kbps** 自适应动态调整（`--desktop-max-bitrate` / `--desktop-min-bitrate`，单位 kbps，默认 800/200）。
- 编码：软件编码（openh264，BSD 许可）兜底；`--desktop-codec h264`。**硬件编码（VAAPI / Windows Media Foundation）按"软编兜底、尽可能硬编"的需求预留为后续扩展**，当前版本为纯软编。
- 下行通道（P2P 直连，阶段1/2）：默认探测顺序 **LAN → WebRTC DataChannel → relay**，全部自动、用户无感：
  - **LAN**：agent 带 `--desktop-lan-port` 时，浏览器与 agent 同网段直接 `http://agent-ip:port/agent/desktop/stream` 拉流（CORS 限定 relay 同源）；
  - **WebRTC**：协商经 relay 信令（`desktop:p2p-*`）完成，DataChannel 承载 fMP4 字节（不可靠模式、丢旧保新）；
  - **relay**：前两者失败/打洞不通时自动回退既有 `/agent/desktop/stream` 转发，功能不劣于旧版。
  - 指标面板"下行通道"行显示当前路径（`lan` / `p2p` / `relay`）。已知限制：P2P 高动态持续帧率受 str0m SCTP cwnd 慢启动限制（LAN/relay 无此问题）；P2P 会话为单活跃 viewer（并发 viewer 自动走 relay）。
- 捕获：X11（含 XWayland）与 Windows GDI 已实现；Wayland 原生捕获需 xdg-desktop-portal + PipeWire 运行时，本版本未内置（agent 会给出明确错误提示，可用 XWayland 走 X11 后端）。
- 传输：fMP4（fragmented MP4）流式推给浏览器 MSE，新加入的观者从最近一个关键帧（IDR）开始接收，无需等待下一个 GOP。
- 其它编码格式（VP8/VP9/HEVC）：考虑二进制体积与许可（x265/libvpx 体积大且 HEVC 浏览器兼容面窄），当前仅内置 H.264——它对全浏览器 MSE 兼容性最好，符合"编码器过大则只选压缩效率足够且浏览器可解"的要求。

### 桌面共享 CLI 示例

```bash
./shell-remote agent --relay-url https://relay.example.com \
  --desktop-capture auto --desktop-fps 15 \
  --desktop-max-bitrate 800 --desktop-min-bitrate 200
```

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

> Linux/macOS 等价命令：`curl -fsSL http://your-relay:3000/agent/install | sh`（运行）或 `... | sh -s -- --download-only`（仅下载）。没有 curl 时可用 `wget -qO- http://your-relay:3000/agent/install | sh`（或 `... | sh -s -- --download-only`）。脚本内部同时支持 curl 与 wget，并从多个 GitHub 镜像自动尝试，以保证在更多网络环境下都能下载成功。

手动启动：

```powershell
# 默认 cmd
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx

# 使用 PowerShell
shell-remote.exe agent --relay-url http://your-relay:3000 --key xxx --shell powershell.exe
```

注意事项：建议以管理员身份运行以完整访问文件系统；交互式程序（ssh/vim）在 MCP exec 路径暂不支持；文件下载（read）正常。终端输出与 MCP 命令结果会自动兼容 Windows 控制台的非常见编码（如 GBK/cp936），统一转换为 UTF-8 后回传，浏览器与 AI Agent 侧无需关心目标机编码。

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
| 文件上传（upload） | ✅ | ✅ |
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

- **标签页布局**：概览 / 会话 / 设备 / 录像 / 访问日志 / 设置 六个标签页；选中页记忆在浏览器（localStorage），刷新后直接回到上次所在页。
- **概览**：版本、运行时间、agent 总数/在线数、浏览器总数。
- **会话监控**：每个会话显示在线状态、最近活跃时间（秒级实时刷新）、Token 列表与权限、连接浏览器数。
- **设备管理**：每台 agent 启动/重连时自动探测并上报主机信息（主机名、架构、系统、内核、CPU 型号）与 agent 版本。"设备"面板逐台展示，支持按关键字（会话ID/主机名/架构/系统/CPU）、架构、在线状态筛选，并可一键连接该设备终端；每台设备可一键触发 **原子自升级**。未上报设备信息的旧版 agent 也能正常列出。
- **访问日志（简易堡垒机审计）**：记录浏览器/MCP 的连接与断开事件（会话、token 前 8 位、权限、时间），最多保留 500 条，可在后台查看最近访问历史。
- **Token 管理**：撤销单个 Token、重生成会话 Token（旧 Token 失效）、切换 Token 权限（rw↔ro）。
- **跳转终端**：每会话"连接"按钮，新标签页打开该会话的浏览器终端（token 预填，服务器密码仍需手填）。
- **会话录制**：`--record-dir` 启用后，交互式终端 I/O（输出+输入）以 asciinema cast v2 落盘；后台显示录制状态，文件可用 `asciinema play` 回放。
- **录像管理**：后台"录像"面板列出全部录制文件（终端录像 + MCP 命令审计）与录制时间/大小；终端录像可直接在后台内嵌播放器回放（xterm.js 渲染，支持播放/暂停、0.25x–4x 慢放快放、时间轴拖拽跳转），MCP 命令记录以结构化卡片查看，均可一键删除。
- **踢出会话**：断开该 agent 及其所有浏览器并撤销其 Token。
- **服务器密码**：查看当前 `--auth`、在线修改（即时生效）。
- **中英文切换**：后台界面右上角切换中/英文（自动探测浏览器语言，localStorage 记忆）。

### 安全说明

- 后台页面不在公开静态资源目录，无法经 `/admin.html` 等路径访问。
- 双层保护：秘密路径（隐藏入口）+ 账户密码登录。
- admin session 仅存内存，relay 重启需重新登录。
- 已知局限：撤销/重生成 Token 后，若 agent 用 `register_existing`（带 Token 重连）重连，可能重新带回该 Token；不影响在线会话。

## Agent 原子自升级

Relay 加 `--agent-upgrade-dir <目录>` 启用；然后把新版本 agent 二进制放入该目录（命名与发布制品一致，见 `scripts/build-releases.sh`）：`shell-remote-x86_64`、`shell-remote-aarch64`、`shell-remote-armv7`，Windows 为 `shell-remote-x86_64.exe`。可选写一个 `shell-remote-<arch>.version` 文件标注版本号（如 `0.19.0`），后台会把它显示为目标版本。

后台"设备"面板每台在线设备都有一个 **升级** 按钮，点击后：

1. relay 按设备上报的架构挑出对应制品，计算 SHA-256，向该 agent 下发 `agent:upgrade`；
2. agent 从 relay 下载制品（凭自身 rw token 鉴权），边下边上报进度（百分比）；
3. 校验 SHA-256 —— 不匹配即失败，保留原二进制；
4. Unix 上先跑一次 `--version` 冒烟测试，确认新制品能在本机执行（防架构不匹配/截断文件）；Windows 跳过（以 SHA-256 为准）；
5. 同目录原子替换（Unix `rename` 原子；Windows 对占用文件的场景回退为辅助 `.bat` 延迟替换），随后以原有启动参数重新拉起新进程并退出旧进程。

升级全程在设备行的"版本"列实时显示状态（已触发 / 下载中 n% / 校验中 / 安装重启中 / 失败原因）；升级完成后显示绿色"已升级 v<版本>"，device 版本列同步刷新。

注意事项：

- agent 所在目录必须可写（`rename` 需要该目录的写权限）；失败会在后台明确提示。
- 重新拉起的新进程会按 `--key` 固定密钥重新注册，身份保持；若原 agent 使用临时令牌启动（未指定 `--key`），重启后会签发新令牌，旧令牌失效。

## 会话录制

relay 加 `--record-dir <目录>` 即可录制交互式终端会话（不含 MCP exec）：

```bash
shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 --record-dir /var/log/shell-remote
```

- 格式：asciinema cast v2（JSONL），可用 `asciinema play xxx.cast` 或 xterm.js 回放。
- 录制输出 + 输入流；**输入流会包含终端里键入的敏感内容（如 sudo 密码），请妥善保护录制目录的文件权限**。
- 每会话一个文件 `{session_id}_{unix时间戳}.cast`；agent 用 `--session-id` 指定后文件名即该 ID。
- 录制在 relay 侧捕获，不影响 agent；踢出/空闲回收会话时文件自动 flush 关闭。

### MCP 命令审计

启用 `--record-dir` 后，每次 MCP `shell_remote` 工具调用也会被审计（与终端录制同开关、同目录），写入每会话文件 `{session_id}_{unix时间戳}.audit.jsonl`，每行一条 JSON：

```json
{"ts":"2026-07-06T12:34:56Z","unix_ms":1783339200000,"session_id":"seSupportBot","token_prefix":"b4e2ec42","permission":"rw","cmd":"ls -la","timeout_ms":30000,"duration_ms":12,"status":"ok","exit_code":0,"stdout_len":1234,"stderr_len":0,"stdout":"...","stderr":"..."}
```

- `status`：`ok` / `timeout` / `disconnected` / `no_agent` / `rejected_readonly`。
- `token_prefix` 只记 token 前 8 位（足以与后台 token 列表对照，不泄露完整密钥）。
- `stdout` / `stderr` 截断到 4KB（`stdout_len`/`stderr_len` 为完整长度）。
- 管理后台概览页每会话显示 `●AUDIT` 标记；踢出/空闲回收会话时审计文件自动 flush 关闭。
- 后台"录像"面板以"MCP 命令"类型列出每份审计文件，点击查看可见结构化的命令卡片（命令、状态、退出码、耗时、权限、token 前缀、stdout/stderr），亦可直接删除。
- **审计文件同样可能含命令及输出中的敏感内容，请保护录制目录的文件权限。**

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

### MCP 端文件传输

大文件上传/下载走专用流式端点（不经 LLM context）：

- 上传：`curl -T localfile -H "X-SR-Token: <token>" "https://relay/agent/mcp/put?path=/remote/path"`
- 下载：`curl -H "X-SR-Token: <token>" "https://relay/agent/mcp/get?path=/remote/path" -o localfile`

token 即 `shell_remote` 的会话 token；上传需 rw token，下载 rw/ro 均可。NAT 下两机间文件传输走此端点（relay 中继，不落盘）。小内容（配置/脚本）直接用 `shell_remote` + heredoc 写入。

**路径语义**：`path` 为绝对路径时按远程文件系统原样使用；相对路径（如 `foo.txt`）相对于 agent 的 `--root`（默认 `$HOME`）解析，与命令执行时的 cwd 基准不同，注意区分。

**断点续传**：下载支持 HTTP Range（`curl -H "Range: bytes=1000-" ...`），中断后可续传；`bytes=-n`（后缀区间）不支持，返回完整文件。Range 响应为 `206`，带 `Content-Range`/`Content-Length`。

**大小限制**：应用层为流式传输、无内置上限，实际上限由部署方反向代理（nginx/openresty）的 `client_max_body_size` 决定，默认常见 50m。若需更大上传，提高代理配置即可，例如：

```nginx
location /agent/mcp/put {
    client_max_body_size 200m;
}
```

**错误码**：`401` token 无效；`400` 缺少/重复 path 参数、路径是目录、相对路径无法解析；`403` 权限不足；`404` 文件不存在；`416` Range 越界；`500` 其他服务错误。所有错误响应均为 `{"error": "..."}` JSON。

## Token 权限模型

| Token 类型 | 终端输入 | 文件操作 | MCP 执行 |
|-----------|---------|---------|----------|
| ReadWrite | ✅ | ✅ | ✅ |
| ReadOnly | ❌ | 列表/读取 | ❌ |

- **临时 Token**：Agent 断开即失效
- **固定密钥**：通过 `--key` 指定，Agent 重连后仍可使用

## 文件管理器

- 面包屑路径导航
- 上传（流式传输，上限由部署方代理配置决定，见上文"大小限制"）
- 下载、删除、重命名、新建文件夹、刷新
- 侧栏宽度可拖拽调整

## 性能与防堵塞

为避免单个大文件传输或大量终端日志堵塞其他会话，relay 做了多层隔离：

- **文件分块传输**：上传/下载按 256KB 切成小块消息流式收发，单条消息永远不大，不会被一条巨型消息占住 worker 线程或撑爆内存。
- **有界 channel**：relay→agent、relay→浏览器 的 SSE 通道均有界（256 条）；满时优先丢弃可丢失的终端输出帧（`terminal:output` 等），保留控制/结果消息，确保一个卡住的消费者不会无限制涨内存拖垮整个 relay 进程。
- **EventBuffer 字节上限**：会话事件回放缓冲除条数上限（1000）外再加 8MB 字节上限，防止大消息或持续日志刷爆回放缓存。
- **背压**：上传分块走背压发送（agent 跟不上时等待而非丢帧）；下载在独立任务中流式发送，不阻塞 agent 主循环的终端输入转发。

代价：某个会话的消费者严重卡住时，该会话的传输可能失败/丢帧，但不会影响其他会话的响应。

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
# 173 passed; 0 failed (含集成测试)
```

## 许可证

MIT
