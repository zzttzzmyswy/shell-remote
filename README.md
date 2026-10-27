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
- **弱网续接** — 终端 shell（PTY）跨 relay 重连存活：断网 / relay 重启 / 半开链路恢复后，正在运行的命令不中断，浏览器重新加入时回放各标签页最近 64KB 输出；agent 上下行都有超时与心跳探活（下行 60s 空闲判死，上行连续 3 次心跳失败判死），重连退避带抖动且健康会话后复位。详见 [docs/weak-network-robustness.md](docs/weak-network-robustness.md)
- **桌面共享** — agent 捕获 X11/Windows 桌面，H.264（openh264 软编）实时编码，动态码率 800–200kbps 自适应，浏览器 MSE 播放；会话页可在"终端/桌面"间任意切换（桌面默认关闭，点击按钮开流）
- **P2P 直连（阶段1/2）** — 桌面流下行优先直连：同网段浏览器经 `--desktop-lan-port` 直连 agent 本地端点（LAN），否则 WebRTC DataChannel（WebRTC），均失败自动回退 relay 转发。三种路径用户无感切换，任何时刻不劣于纯 relay
- **网页映射** — 会话页"网页"按钮经 relay/agent 隧道浏览远端设备网络中的页面（可直接访问设备自身 `127.0.0.1` 服务）。两种模式按能力与 Token 权限自动选择：**v2 完整模式**（`net:tcp-v1` + rw + 安全上下文）支持脚本、SPA 路由、Cookie 登录、POST 与 WebSocket；**v1 静态模式**（`web:proxy-v1`）只渲染静态页面；两者都没有的旧版 agent 完全不显示入口。详见[网页映射](#网页映射)
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

### 两个二进制：CLI 与 UI

v0.51.0 起拆分为两个二进制，共享同一 relay 与协议：

| 二进制 | 子命令 | 能力 | 典型用途 |
|--------|--------|------|----------|
| `shell-remote`（CLI，lean） | `relay` / `agent` | agent **仅终端转发**（PTY + 文件管理器 + MCP exec）；无任何桌面代码 | 自建 relay 服务器；NAS/云主机/无显示器设备的 headless agent；MCP 集成 |
| `shell-remote-ui`（UI） | `agent` | **终端 + 桌面转发**（全部 `--desktop-*` 参数） | 需要共享真实桌面的设备 agent（SE7 等带桌面环境） |

- CLI 二进制构建时不编译桌面代码（`--no-default-features`）：无 openh264/libaom/x11rb/str0m 依赖，release 体积约 **6-7 MB**（桌面版约 25 MB+），交叉编译更快。
- 需要桌面共享的设备请下载并使用 `shell-remote-ui-<arch>`，安装/自升级均按该文件名提供。
- 协议、relay、web UI 完全通用：两类 agent 可同时接入同一 relay。

### 下载预编译二进制

[GitHub Releases](https://github.com/zzttzzmyswy/shell-remote/releases) 为每个平台提供两种 musl 静态编译二进制：

```bash
# x86_64 (Intel/AMD)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-x86_64 && chmod +x shell-remote-x86_64
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-ui-x86_64 && chmod +x shell-remote-ui-x86_64

# aarch64 (ARM 64位, 树莓派4/5, 云服务器)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-aarch64 && chmod +x shell-remote-aarch64
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-ui-aarch64 && chmod +x shell-remote-ui-aarch64

# armv7 (ARM 32位, 树莓派2/3)
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-armv7 && chmod +x shell-remote-armv7
curl -fLO https://github.com/zzttzzmyswy/shell-remote/releases/latest/download/shell-remote-ui-armv7 && chmod +x shell-remote-ui-armv7
```

### 编译

```bash
git clone https://github.com/zzttzzmyswy/shell-remote.git && cd shell-remote
# CLI（relay + agent 终端；不含桌面代码）
cargo build --release
# UI（agent 终端 + 桌面）——desktop feature 默认开启，产物为 target/release/shell-remote-ui
cargo build --release --bin shell-remote-ui
# 纯终端可选：不编译任何桌面依赖（等价于 release 的 CLI 产物更小）
cargo build --release --no-default-features
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
| `--download-dir` | 无 | 离线二进制分发目录：目录内按文件名（如 `shell-remote-x86_64`、`shell-remote-aarch64`、`shell-remote-armv7`、`shell-remote-x86_64.exe`，以及同名的 `shell-remote-ui-*` 桌面版）放置各平台 agent 二进制，经 `/download/<文件名>` 对外提供；**安装脚本会优先从本 relay 下载**，GitHub 镜像仅作回退（适合内网/镜像受限环境） |
| `--agent-upgrade-dir` | 无 | agent 自升级制品目录（`shell-remote-<arch>[.exe]` 与 `shell-remote-ui-<arch>[.exe]`，可选 `shell-remote-<arch>.version` 标注版本）；不设则"设备"页升级功能不可用 |
| `--bandwidth-down-mbps` | `0`（不限） | relay 整体**下行**（relay 出站）带宽上限，单位 Mbit/s。优先级：settings 文件里的同名字段 > 本参数 > 不限。后台"设置 → 带宽限制"修改后即时生效并写回 settings 文件 |
| `--bandwidth-up-mbps` | `0`（不限） | relay 整体**上行**（relay 入站）带宽上限，单位 Mbit/s，优先级同上 |
| `--settings-file` | `~/.shell-remote/relay-settings.json` | 带宽设置的持久化文件（JSON）。后台每次保存都会先写临时文件再 rename 覆盖（原子写）；文件损坏或缺失时按"未配置"处理并退回 CLI 值 |

**带宽限制的行为**（详见下方「带宽限制」一节）：文件传输、升级包、二进制下载走等待式限速（只降速、不丢数据）；桌面视频令牌不足时丢帧并回传 `desktop:congested`（带宽丢帧不计入踢 viewer 的计数，agent 会自动降码率）；终端与控制等交互消息只记账不等待，保证响应速度。

### 启动 Agent（CLI，终端转发）

```bash
./shell-remote agent --relay-url https://<relay-ip>
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--relay-url` | `http://localhost:3000` | Relay 地址（HTTPS 或 HTTP，使用 SSE+POST 协议） |
| `--relay-insecure` | — | 信任自签证书（连自签 TLS 的 relay 时必填，否则 register 握手即失败） |
| `--key` | — | 固定鉴权密钥（不指定则随机生成临时 Token） |
| `--root` | `$HOME` | 文件管理器默认目录 |
| `--token-type` | `rw` | Token 类型：`rw`、`ro` 或 `both` |
| `--shell` | `/bin/bash` | Shell 路径。注意该参数绑定了 `SHELL` 环境变量，在 systemd 下会被服务注入的 `SHELL`（该账户登录 shell）顶掉，需要固定时请显式传参 |
| `--session-id` | — | 自定义会话 ID（5-20 位字母数字），后台据此区分设备；**可重复使用**——新的 agent 用相同 ID 注册会顶替旧会话（旧 Token 失效），不再报冲突 |
| `--no-cmd-log` | — | 关闭命令日志（默认开启：把 MCP 执行的命令与 Web 终端输入的命令行打印到日志） |

> CLI agent **不提供桌面转发**（无 `--desktop-*` 参数，能力上报 `available:false`，浏览器端不显示桌面按钮）。需要桌面共享请用 **shell-remote-ui**。

> 要把 agent 装成**开机自启的 systemd 服务**（固定运行账户 / 固定 session-id 与 key /
> 预置远端终端环境变量），见 [`deploy/systemd-agent/`](deploy/systemd-agent/)。

### 命令日志

agent（`shell-remote agent` 与 `shell-remote-ui agent` 两个二进制）**默认**把执行过的命令
按统一格式打进日志，方便事后追溯"这条机器上到底跑了什么"。两个来源格式对齐，单行、字段
间两个空格：

```
[MCP] exec        sess=ABCDE  shell=auto  cwd=/root  timeout=30s  $ ls -la
[MCP] exec_start  sess=ABCDE  $ tail -f /var/log/syslog
[MCP] stdin       sess=ABCDE  id=1a2b3c4d  bytes=4  "yes\n"
[WEB] cmd         sess=ABCDE  tab=tab-1  $ git status
[WEB] cmd~        sess=ABCDE  tab=tab-2  $ systemctl restart nginx
```

| 字段 | 含义 |
|------|------|
| 动作名 | `exec` / `exec_start` / `stdin` 来自 MCP；`cmd` / `cmd~` 来自网页终端 |
| `sess=` | 设备会话 ID（后台看到的那个） |
| `shell=` / `cwd=` / `timeout=` | MCP 请求里显式指定的值；未指定写 `-`。超时整秒写 `30s`，非整秒写 `1500ms` |
| `id=` / `bytes=` | stdin 目标会话 ID 的前 8 位 + 原始字节数 |
| `tab=` | 网页终端标签页 ID |
| `$ ` 之后 | 命令文本，永远是最后一个字段；换行/制表符/控制符转义成 `\n` `\t` `\x1b`，超过 1000 字符截断 |

`cmd~` 表示这一行在编辑过程中用过方向键、Tab 补全或历史检索——agent 侧只能按键重建，
内容可能与实际执行的不完全一致。精确的命令请以 `exec` 系列（MCP）为准。

**关闭**：加 `--no-cmd-log`。

```bash
./shell-remote agent --relay-url https://<relay-ip> --no-cmd-log
```

**日志位置**：沿用现有 tracing 配置——默认输出到 stderr；设 `SR_LOG_DIR` 时写该目录下
按小时滚动的文件；`shell-remote-ui` 的窗口/TUI 交互模式会自动落到
`~/.shell-remote/`，避免刷屏污染界面。

**限制**

- 网页终端只有在 shell 提示符下的输入才记录。密码输入（`sudo`/`ssh`/`read -s`，表现为
  回显关闭 + 行缓冲）和 `vim`/`python`/`ssh` 等前台程序里的按键都不记录——前者防泄密，
  后者防噪声。
- 上述判定在 Unix 上通过终端 termios 与前台进程组实现；**Windows 的 ConPTY 拿不到这些
  信息，所以 Windows agent 会记录全部终端输入**（包括密码输入）。在 Windows 上处理敏感
  操作时请加 `--no-cmd-log`。

### 启动 UI Agent（桌面共享）

```bash
./shell-remote-ui agent --relay-url https://<relay-ip>
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--relay-url` | `https://localhost:3000` | Relay 地址（HTTPS 或 HTTP，使用 SSE+POST 协议） |
| `--key` | — | 固定鉴权密钥（不指定则随机生成临时 Token） |
| `--root` | `$HOME` | 文件管理器默认目录 |
| `--token-type` | `rw` | Token 类型：`rw`、`ro` 或 `both` |
| `--shell` | `/bin/bash` | Shell 路径 |
| `--session-id` | — | 自定义会话 ID（5-20 位字母数字），后台据此区分设备；可重复使用 |
| `--no-cmd-log` | — | 关闭命令日志（默认开启，格式与限制见上文「命令日志」） |
| `--desktop-capture` | `auto` | 桌面捕获后端：`auto` / `dxgi` / `gdi` / `x11` / `wayland` / `none`（Windows 默认 DXGI Desktop Duplication，失败回退 GDI；`wayland` 需 `--features wayland` 构建且运行时存在 xdg-desktop-portal + PipeWire） |
| `--desktop-codec` | `av1` | 桌面编码格式：`av1`（libaom）/ `h264`（openh264）；初始化失败自动按 av1→h264 回退 |
| `--desktop-fps` | `30` | 桌面编码帧率上限（默认 30，QoS 内容驱动：静止 1fps、动态满帧） |
| `--desktop-capture-fps` | `0` | 抓帧独立上限（fps）：0 = 不限速（编码 min_gap 跳帧兜底） |
| `--desktop-max-bitrate` | `0`（自动） | 最大编码码率（kbps，0 = 自动按 base_bitrate×质量档） |
| `--desktop-min-bitrate` | `80` | 最小编码码率（kbps，静态桌面足够；动态由 ABR 拉回） |
| `--desktop-quality` | `balanced` | 编码质量档：`speed` / `balanced` / `best` |
| `--desktop-display` | 平台默认 | Linux：X11 display（如 `:1`，默认取 `$DISPLAY`）；Windows：显示器枚举序号（`"0"`=主屏、`"1"`=第 2 块屏，MYS-954 多屏选屏，会话页显示器下拉运行时可切） |
| `--desktop-lan-port` | `0` | LAN 直连桌面流监听端口（阶段2）。`0` = 不启用；非 0 时同网段浏览器直接 `http://agent-ip:port/agent/desktop/stream` 拉流，绕开 relay |

输出示例：

```
session: a1b2c3d4
  rw: 5fe42fc877b0a721157508c67fd19633c9c03cc97aaa2d5af0ced67cd3980d90
```

- `session:` — 8 位会话 ID（仅用于日志）
- `rw:` / `ro:` — Token（浏览器登录或 MCP 调用使用）

### UI Agent 终端状态面板（TUI）

在**终端**（TTY）中运行 `shell-remote-ui agent`（非后台/systemd）时，自动显示实时状态面板：

```
┌ shell-remote-ui ─────────────────────────────────────────┐
│状态: 已连接    会话 id: e2eui1    已运行: 00:12:34         │
│Relay: https://relay.example.com                           │
│延迟: 23 ms（经心跳 ping，每 15s 更新）                    │
│ Token                                                    │
│  rw: 0123…cdef                                            │
│  ro: 4567…89ab                                            │
└ [r] 刷新 token（会话 id 不变）   [q] / Ctrl-C 退出 ────────┘
```

- 展示：连接状态（启动中/连接中/已连接/重连中）、会话 id、token（rw/ro）、relay 地址、relay 延迟（心跳 RTT，约 15s 更新一次）、本次连接已运行时长。
- **`r`**：手动刷新 token —— 重新注册换取新 token，**会话 id 保持不变**；旧 token 立即失效（已被新版顶替）。仅在非 `--key`（随机 token）模式下 token 才会变化；`--key` 模式下 token 即 key，固定不变。
- **`q` / `Esc` / `Ctrl-C`**：干净退出（还原终端 → 请求 agent 停机，PTY 子进程由 drop 回收，不残留孤儿进程）。
- 无 TTY（systemd/nohup/重定向）时自动保持原有 headless 行为，不渲染面板；此时业务日志若未设 `SR_LOG_DIR` 会默认落到 `~/.shell-remote/`（TUI 模式下防日志污染界面）。

### UI Agent 原生窗口面板（GUI，v0.53.0+）

带桌面的设备（`DISPLAY` 已设置，或 Windows）运行 `shell-remote-ui agent` 时，自动打开**原生窗口**状态面板（egui/eframe），内容与 TUI 一致：状态/会话 id/token/relay 地址/relay 延迟/已运行时长，按钮"刷新 token（会话 id 不变）"与"退出"。

```
┌ shell-remote-ui ────────────────────────────────┐
│ shell-remote-ui 状态                            │
│ 状态：已连接    会话 id：e2eui1                  │
│ Relay：https://relay.example.com                │
│ 延迟：23 ms（经心跳 ping，每 15s 更新）          │
│ 已运行：00:12:34                                │
│ Token：                                        │
│   rw: 0123…cdef                                 │
│   ro: 4567…89ab                                 │
│ [刷新 token（会话 id 不变）] [退出]             │
└────────────────────────────────────────────────┘
```

- **视图自动选择**：有 `DISPLAY`（Linux 桌面环境）/Windows → 原生窗口；否则有 TTY → 终端面板；都无 → headless。可用 `--view <auto|window|tui|headless>` 强制指定。
- **静态链接**：窗口 GUI 走 winit X11（纯 Rust 协议 x11rb），libxkbcommon/libGL 运行时 dlopen，二进制保持 **musl 全静态**（aarch64/armv7 构建脚本自动加空 `libdl.a` stub）。Windows 静态 exe 原生支持窗口。
- 中文显示：自动发现系统 CJK 字体（Windows 微软雅黑 / Linux Noto CJK / WQY / 文鼎等，按常见路径与字体目录扫描）。
- 无 `DISPLAY` 且无 TTY 时保持 headless；业务日志默认落 `~/.shell-remote/`。

### 浏览器访问

打开 `http://<relay-ip>:3000`，输入服务器密码及 Token 即可连接。主区域为 xterm.js 终端，右侧为文件管理器。

## 桌面共享

在 agent 模式下共享设备真实桌面：浏览器端"桌面"按钮开流，画面实时编码后经 relay 转发播放（`/agent/desktop/stream`）。

- 启动：浏览器会话页点击工具栏"桌面"按钮（默认关闭、点击才开流）→ 收到 `desktop:started` 后自动连接视频流；再点"终端"切回。
- 权限：开流/关流需 rw Token（`requires_write`）；观看桌面画面 rw/ro 均可。
- 码率：自适应动态调整（`--desktop-max-bitrate` / `--desktop-min-bitrate`，单位 kbps；默认 0 = 自动按 rustdesk 模型 base_bitrate×质量档，1080p balanced ≈1388kbps）。
- 编码：**纯软件编码**，默认 AV1（libaom），可切 VP9（libvpx）/ VP8 / H.264（openh264）——`--desktop-codec`；初始化失败按 av1→vp9→vp8→h264 自动回退。**不调用 GPU 编码**（Windows 上 DXGI 只用于屏幕捕获，编码全在 CPU；D3D11 设备在无可用 GPU 驱动时自动回退 WARP 软光栅），带显卡的 Windows 与无显卡环境行为一致。硬件编码（VAAPI / Media Foundation）预留为后续扩展。
- 灰度模式：会话页桌面控制栏"灰度"开关，弱网下省带宽。AV1 下切换为 **libaom 原生 monochrome 码流**（色度平面不进码流，实测比彩色省 ~30% 码率，重建流生效）；VP9/H.264 下编码前把色度置中性（下一帧即时生效）。
- 多显示器：`desktop:started` 上报远端显示器拓扑（X11 RANDR 输出 / Windows DXGI 输出 / GDI 显示器枚举），指标面板"远端显示器"行可见；多屏时会话页出现"显示器"下拉，选择后重建桌面流切换到对应屏幕（Windows 上 `"0"`=主屏、`"1"`=第 2 块屏，以此类推）。
- 下行通道（P2P 直连，阶段1/2）：默认探测顺序 **LAN → WebRTC DataChannel → relay**，全部自动、用户无感：
  - **LAN**：agent 带 `--desktop-lan-port` 时，浏览器与 agent 同网段直接 `http://agent-ip:port/agent/desktop/stream` 拉流（CORS 限定 relay 同源）；**部署限制**：该 LAN 拉流为 `http://` 混合内容——relay/会话页以 **HTTPS** 提供时浏览器会直接拦截此 fetch，Stage-2 LAN 直连不可用（无挂起、快速回退 P2P/relay），LAN 通道仅在 relay/会话页走纯 HTTP 时生效；WebRTC P2P 与 relay 回退不受影响；
  - **WebRTC**：协商经 relay 信令（`desktop:p2p-*`）完成，DataChannel 承载 fMP4 字节（不可靠模式、丢旧保新）；
  - **relay**：前两者失败/打洞不通时自动回退既有 `/agent/desktop/stream` 转发，功能不劣于旧版。
  - 指标面板"下行通道"行显示当前路径（`lan` / `p2p` / `relay`）。已知限制：P2P 高动态持续帧率受 str0m SCTP cwnd 慢启动限制（LAN/relay 无此问题）；P2P 会话为单活跃 viewer（并发 viewer 自动走 relay）。
- 捕获：Windows DXGI Desktop Duplication（默认，多屏可选）→ GDI BitBlt 回退（多屏可选）；Linux X11（含 XWayland）与 Wayland 原生（portal + PipeWire，需 `--features wayland` 构建）。

### 桌面共享 CLI 示例

```bash
./shell-remote-ui agent --relay-url https://relay.example.com \
  --desktop-capture auto --desktop-fps 30
```

## 网页映射

会话页工具栏的"网页"按钮打开一个独立视图，经 relay → agent 隧道访问**远端设备网络环境**中的网页——包括只监听 `127.0.0.1`、外部无法直接访问的设备自身服务。

有两种模式，按 agent 能力、Token 权限和浏览器安全上下文自动选择：

| 模式 | 条件 | 能力 |
|------|------|------|
| **v2 完整模式** | agent 声明 `net:tcp-v1` + **rw** Token + 安全上下文（受信证书的 https，或 `http://localhost` / `http://127.0.0.1`） | 远端脚本执行、SPA history 路由、Cookie 登录、任意方法与请求体、表单提交、**WebSocket**、下载 |
| **v1 静态模式** | agent 只声明 `web:proxy-v1`（v0.58.x），或 v2 条件不满足但 v1 可用 | 静态/轻量页面：远端 JS 不执行，只转发单次 GET |
| 无入口 | agent 两个能力都不声明（≤ v0.57.0） | — |

网页视图的地址栏右侧显示当前模式徽标（"完整模式" / "静态模式"）；降级时提示区会写明原因（只读 Token / 非安全上下文 / agent 版本旧）。

### 能力协商与模式判定

入口与模式完全由 `session:capabilities` 里声明的能力决定，不按版本号猜测：

- agent 注册时在 `capabilities` 里声明能力；relay 在浏览器 SSE 握手时补发 `session:capabilities {capabilities:[...]}`。
- 浏览器侧判定（`web/webview.js` 的 `detectMode`，纯函数、可单测）：
  1. 含 `net:tcp-v1`，且 `sessionStorage['shell-remote-permission'] === 'rw'`，且 `window.isSecureContext && 'serviceWorker' in navigator` → **v2**；
  2. 否则含 `web:proxy-v1` → **v1**（并记录降级原因）；
  3. 都没有 → 入口隐藏。
- 每次重连都会重新收到该事件，因此能力消失（agent 回退版本）会立刻把入口收起。
- `session:agent_disconnect`（agent 断线）时立即隐藏入口、退出网页视图、清空 iframe，并通知 Service Worker 丢弃该会话的页面归属；重连后以新的能力数组为准。

### v2 完整模式

#### 接口

| 路径 | 方法 | 鉴权 | 说明 |
|------|------|------|------|
| `/agent/web/map` | POST | `X-SR-Token` + `X-Auth` | 建映射，body `{"url":"<绝对 http/https URL>"}`，返回 `{map_id, base, target_origin, expires_in}` |
| `/agent/web/m/<map_id>/…`（含无尾斜杠的 `/agent/web/m/<map_id>`） | ANY | 无（`map_id` 本身就是访问凭据） | 反代：透传方法、头、body、Cookie；`Upgrade: websocket` 时透传 101 |
| `/sr-web-sw.js` | GET | 无 | Service Worker 脚本（scope `/`） |
| `/sr-web-shim.js` | GET | 无 | 注入到映射页面的脚本 |

- 同一会话同一目标复用同一 `map_id`。
- 状态码：ro Token 403、agent 无 `net:tcp-v1` 404 `{"error":"unsupported"}`、agent 离线 503、URL 非法 400。
- 带 `Service-Worker: script` 头经反代请求 → 403（禁止远端页面借映射路径注册 SW）。
- 响应改写：`Location` 加映射前缀；`Set-Cookie` 去 `Domain`、`Path=/` 保持 `/`、其他 `Path` 加前缀；剥掉 CSP / XFO / HSTS / `Clear-Site-Data` / COEP / COOP / CORP / `Alt-Svc` / `Service-Worker-Allowed`。
- 反代只搬字节：agent 侧是纯 TCP 管道，不解析 HTTP、不改写响应体。

#### 浏览器侧路由（Service Worker）

`web/sr-web-sw.js` 在安全上下文注册（scope `/`），把映射页面的请求改写到反代路径。**映射页面在 iframe 中以目标站原始路径运行**（`location.pathname` 里没有 `/agent/web/m/<id>` 前缀），因此根路径资源、history 路由、`<a href="/x">` 整页跳转都按目标站点的写法工作。

fetch 事件分流（判定为"不属于映射"时**直接 `return`、不 `respondWith`**，shell-remote 自身的页面 / SSE / 上传零影响）：

1. 保留路径 `/sr-web-sw.js`、`/sr-web-shim.js` 永远放行。
2. **导航请求**只处理 `destination` 为 `iframe` / `frame` 的；顶层 `document` 导航一律放行，绝不劫持会话页自己。
   - 路径以 `/agent/web/m/<id>/` 开头（会话页首次进入，或 relay 改写后的 3xx `Location`）→ 登记 `pendingNav[去前缀的绝对 URL]`（10s 过期）后 `302` 到该 URL；
   - 其他 iframe 导航按 `pendingNav` → `clientId` / `replacesClientId` 归属 → `lastActive`（30 min 内）的顺序确定 `map_id`，没有就放行；命中后向 `/agent/web/m/<id>` + 原始 pathname + query 发 fetch（`redirect:'manual'`）；
   - HTML 响应**按字节**在第一个 `<head>` 标签的 `>` 之后插入 `<script src="/sr-web-shim.js?m=…&o=…">`；前 64 KiB 内找不到 `<head>` 就插在最前面。全程用 `TransformStream` 处理字节流，**不做 TextDecoder 解码**——内网设备的 GBK 页面解码再编码会损坏中文。
3. **非导航请求**只处理来自映射页面的（`clientId` 在归属表里）：同源 URL 或 host 等于 `target_origin` 的 host（兼容浏览器把 http 自动升级成 https）→ 加映射前缀后重发；其他跨源放行。

状态（`maps` / `clients` / `pendingNav` / `lastActive`）全部落 IndexedDB（库 `sr-web`），并按"clientId 已不在 `clients.matchAll()`"与"超过 24h"定期清理——Service Worker 随时可能被浏览器回收，内存状态不可依赖。

#### 注入脚本

`web/sr-web-shim.js` 由 SW 插在页面第一个 `<head>` 之后同步执行，补齐 SW 管不到的部分：

- **WebSocket**：`window.WebSocket` 换成子类（保持 `instanceof`、静态常量、`protocols` 参数）。host 等于本站 host（且未带映射前缀）或等于 `target_origin` 的 host → 改写为 `(https?→wss?:)//<本站 host>/agent/web/m/<id>` + pathname + query；其他源原样。`ws` / `wss` 由**本页协议**决定（同源方案下 TLS 由 relay 在隧道上做）。
- **fetch / XMLHttpRequest.open / EventSource**：只把指向 `target_origin` 的**绝对** URL 改成同源相对路径（`pathname + query`），之后由 SW 接手。相对的与 `location.host` 的写法本来就走 SW，不动。
- **Service Worker 防护**：`navigator.serviceWorker` 的 `register` / `unregister` / `update` 返回 rejected Promise（`service workers are disabled inside shell-remote web mapping`），`getRegistrations` → `[]`，`getRegistration` → `undefined`，用 `Object.defineProperty` 定在实例上（不改原型——同源方案下改原型会连会话页自己的注册一起废掉）。远端页面因此注销不掉我们的 SW，也抢不走本站源的请求代理权。
- 自包含 IIFE，不依赖 shell-remote 的全局变量，不读写 `sessionStorage` / `localStorage`。

#### 部署要求

1. **安全上下文**：Service Worker 只能在受信证书的 https，或 `http://localhost` / `http://127.0.0.1` 下注册。用默认自签证书访问 relay 时浏览器不认，自动回落到 v1 静态模式。
2. **前置 nginx 必须透传 WebSocket 并关闭缓冲**（否则 WS 握手 400、HTML 注入因缓冲而延迟）：

```nginx
location /agent/web/m/ {
    proxy_pass http://127.0.0.1:3000;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_buffering off;
}
```

`/sr-web-sw.js` 与 `/sr-web-shim.js` 走普通静态路径即可，但**不要**加 `Cache-Control: immutable` 长缓存——SW 脚本用 `updateViaCache: 'none'` 注册，服务端按内容 ETag + `no-cache` 返回。

#### 安全说明

- 映射页面与 shell-remote **同源**运行（用户已明确接受该风险的取舍，不使用独立域名/端口）。因此远端页面的脚本**能读到会话 Token 与服务器口令**（`sessionStorage`），等同于把该设备的 shell 交给它。**只访问可信设备。**
- 完整映射能提交表单、修改路由器等设备配置，因此**只对 rw Token 开放**；ro Token 继续用 v1 静态模式。
- relay 侧禁止远端页面经映射路径注册 Service Worker（`Service-Worker: script` → 403），并剥掉会作用到本站源或挡住同源嵌入的响应头。
- 反代不外发 `X-SR-*` 凭据头与前置反代添加的客户端 IP 头（`X-Forwarded-*` / `X-Real-IP` / `Forwarded`）。

### v1 静态模式（v0.58.x 回退路径）

| 路径 | 方法 | 鉴权 | 说明 |
|------|------|------|------|
| `/agent/web/ticket` | POST | `X-SR-Token` + `X-Auth` | 签发绑定会话的短期凭据，返回 `{ticket, expires_in}`（默认 600s） |
| `/agent/web/proxy?ticket=…&url=…` | GET | 仅 ticket（签发时已鉴权） | 取远端页面；响应头 `X-SR-Final-URL` 给出重定向后的最终地址 |

- 只支持 GET；`url` 必须是绝对 `http://` / `https://` 地址，长度 ≤ 4096。
- **ticket 与会话 Token 完全独立**：256 bit CSPRNG 随机、不可从 Token 推导、每会话最多 8 个、默认 10min 过期、agent 断线时全部撤销。ticket 只出现在代理 URL 的 query 里，**会话 Token 与服务器口令从不进入任何 URL**。
- 状态码：无效会话 Token 401、无效/过期 ticket 403、agent 无该能力 404、agent 离线 503、并发超限 429。
- ro Token 也能浏览（页面 GET 不改变远端状态）。

页面加载策略：relay 只做流式转发，重写全部在浏览器端 `web/webview.js`：用 `X-SR-Final-URL`（与文档内 `<base href>`）作相对 URL 基准 → 重写 `src` / `href` / `poster` / `srcset` / `style` 与 CSS `url()` / `@import` 指向同源代理 URL → 外部样式表取回后内联 → 删除 `<script>`、远端 CSP/refresh `<meta>` 与 `<base>` → 结果放进 `iframe[srcdoc]`，sandbox 只用 `allow-same-origin`（**不给 `allow-scripts`**，远端 JS 一律不执行）。站内链接点击被父页面拦截后改走同一代理；表单提交被拦下并提示。

### 使用示例

会话页点"网页" → 地址栏填设备侧地址 → "打开"。地址在**远程设备侧解析**，因此可以填：

```
http://127.0.0.1/                  # 设备自身服务（默认地址）
http://127.0.0.1:8080/admin        # 设备上另一端口
http://192.168.1.50/status         # 设备所在内网的其它主机
https://internal.example.local/    # 内网 HTTPS（v2 由 relay 在隧道上做 TLS，接受自签证书）
```

v2 打开页面后，地址栏显示的是**目标站点的原始地址**（会随页面内的路由与跳转更新）；"刷新"执行 iframe 内部 reload，保留当前路由与登录态。直接在地址栏输入另一个源的地址会为该源新建一个映射。

### 已知限制

v2（完整模式）：

- **同一浏览器多个标签页同时映射不同设备时，整页跳转只能尽力归属**（按 `pendingNav` → clientId → 最近活跃映射的顺序），可能落到另一个标签页的映射上。子资源与 `fetch` 按页面（clientId）准确归属，不受影响。
- 页面里的 `window.open` 弹窗与**顶层**跳转不经过映射（SW 明确不劫持顶层导航）。
- 写死目标 IP 的静态资源（`<img src="http://192.168.0.1/x.png">`）会被浏览器的混合内容策略拦截；相对地址与 `location.host` 的写法不受影响。
- 非受信证书的 https relay（默认自签）无法注册 SW → 自动回落 v1。
- 目标站自身注册 Service Worker 会被 shim 屏蔽（同源方案下的必然取舍）。

v1（静态模式）：

- 远端 JavaScript 不执行，依赖 JS 渲染的 SPA、懒加载、动态拼出来的 URL 都不工作。
- WebSocket / SSE 等长连接、非 GET 表单、文件下载不支持。
- 自签证书 / 双向 TLS 的目标站会被 agent 的 HTTP 客户端拒绝（rustls 默认根证书）。
- 单请求 5min 总时限、块间 30s 空闲超时；单页并发子资源每会话上限 8 个在途请求，超出返回 429。
- 已内联的样式表上限 8 个 / 单个 512 KiB，主文档上限 8 MiB；超限时提示而不静默显示半页。

### 验证

```bash
# 纯函数单测（node，无框架）：SW 的 URL 判定 / 前缀剥离 / <head> 字节注入 / WebSocket 改写
node tools/test_sr_web.js
node tools/test_webview.js

# 用真 relay + 真 agent + 只监听 127.0.0.1 的测试 SPA 做端到端验证（无头 Chromium）：
# SPA 渲染、history 路由、Cookie 登录、整页跳转、302、WebSocket echo 与推送、
# 绝对地址 fetch、GBK 页面、页面尝试注销 SW、SW 安装后 shell-remote 自身回归、
# ro / v0.58 / v0.57 三种降级、agent 断线。
cargo build --bin shell-remote
xvfb-run -a node tools/verify_web_map_ui.js
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
# CLI（终端）
cargo build --release --target x86_64-pc-windows-gnu
# UI（终端 + 桌面）
cargo build --release --bin shell-remote-ui --target x86_64-pc-windows-gnu
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
| `/agent/web/ticket` | POST | 网页映射 v1：签发绑定会话的短期资源凭据（`X-SR-Token` + `X-Auth`） |
| `/agent/web/proxy` | GET | 网页映射 v1：经隧道取远端页面（`?ticket=…&url=…`，返回 `X-SR-Final-URL`） |
| `/agent/web/map` | POST | 网页映射 v2：建（或复用）映射，body `{"url":…}`（仅 rw Token） |
| `/agent/web/m/:map_id/*rest` | ANY | 网页映射 v2：透明反代（含 WebSocket 透传） |
| `/sr-web-sw.js` | GET | 网页映射 v2：Service Worker（scope `/`） |
| `/sr-web-shim.js` | GET | 网页映射 v2：注入到映射页面的脚本 |
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

Relay 加 `--agent-upgrade-dir <目录>` 启用；然后把新版本 agent 二进制放入该目录（命名与发布制品一致，见 `tools/build-dist.sh`）：CLI 为 `shell-remote-x86_64`、`shell-remote-aarch64`、`shell-remote-armv7`，Windows 为 `shell-remote-x86_64.exe`；桌面版 UI agent 为对应的 `shell-remote-ui-<arch>[.exe]`。可选写一个 `shell-remote-<arch>.version` 文件标注版本号（如 `0.19.0`），后台会把它显示为目标版本。

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

### 登录环境（`/etc/profile`）

agent 由 systemd/服务直接启动，**不经过 PAM/SSH**，子进程只有 agent 自身的环境。
设备上很多工具的 PATH 是写在 `/etc/profile` 里的（例如 Sophon 的 `bm-smi` 位于
`/opt/sophon/libsophon-current/bin`），所以网页终端和 MCP 默认都读不到它们——
不需要再手动 `source /etc/profile`，两条路径现在都以**登录 shell**方式启动：

| 路径 | 实际启动方式 |
|---|---|
| 网页终端 | `<--shell 指定的 shell> -l`（如 `/bin/bash -l`） |
| MCP `exec` / `exec_start` | `sh -l -c <你的命令>` |

- **不改写你的命令**：命令始终作为一个独立 argv 原样交给 `sh`，不拼接、不加前缀。
- **不是 `ENV`/`BASH_ENV`**：POSIX 的 `ENV` 只在**交互** shell 里被读取，实测
  `ENV=/etc/profile sh -c '...'` 拿不到任何变量，因此走 `-l` 这一登录 shell 标准语义。
- **`/etc/profile` 不存在时不影响任何功能**：shell 自行跳过，命令照常执行。
- **只影响 Unix**：Windows 行为不变（无 `/etc/profile` 概念）。
- **不注入 locale**：agent 仍然不设置 LANG/LC_*，目标机自己的 locale 原样生效。

> ⚠️ 登录环境的既有语义：如果 `/etc/profile` 或其 `profile.d` 里有**只适合交互登录**
> 的输出/命令（打印欢迎语、启动 agent、设置仅终端有意义的变量等），MCP 的结果里
> 也可能看到这些输出。这是登录 shell 的固有行为，不是 bug；遇到干扰请调整 profile。

- 若 `--shell` 指定的 shell 不接受 `-l`（如 `tcsh`/`elvish`/`nu`），agent 启动时会
  自动探测并退回原来的非登录启动，网页终端照常可用，只是不加载 `/etc/profile`。
  探测结果按 shell 路径缓存，每个进程只做一次。

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
