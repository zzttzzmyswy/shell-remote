# 管理后台与 HTTP 端点

## 管理后台

Relay 可选启用一个 web 管理后台。默认禁用，需命令行显式开启；**首页不显示入口，必须手动输入秘密子路径才能到达**。

### 启用

```bash
./shell-remote relay --auth YOUR_PASSWORD --bind 0.0.0.0:3000 \
  --admin-path /your-secret-path --admin-pass ADMIN_PASSWORD
```

`--admin-user` 默认 `admin`。设了 `--admin-path` 就必须给 `--admin-pass`，否则 relay 拒绝启动；不设 `--admin-path` 时后台路由完全不注册，面板无法访问。

### 访问

浏览器打开 `https://<relay-ip>:3000/your-secret-path`（即 `--admin-path` 的值），用 `--admin-user` / `--admin-pass` 登录。秘密路径是第一道屏障；登录后获发 HttpOnly + SameSite=Strict 的 session cookie，有效期 12 小时，仅存内存，relay 重启需重新登录。

### 功能

- **标签页布局** — 概览 / 会话 / 设备 / 录像 / 访问日志 / 设置六页，选中页记忆在浏览器 localStorage
- **概览** — 版本、运行时间、agent 总数 / 在线数、浏览器总数、带权限的 Token 列表
- **会话监控** — 在线状态、最近活跃时间（秒级刷新）、Token 列表与权限、连接浏览器数
- **设备管理** — agent 启动/重连时自动探测上报主机信息（主机名、架构、系统、内核、CPU 型号）与 agent 版本。支持按关键字（会话 ID / 主机名 / 架构 / 系统 / CPU）、架构、在线状态筛选，逐台一键连接终端，逐台一键触发原子自升级（见 [upgrade.md](upgrade.md)）。未上报设备信息的旧版 agent 也能正常列出
- **访问日志（简易堡垒机审计）** — 记录浏览器 / MCP 的连接与断开事件（会话、Token 前 8 位、权限、时间），最多保留 500 条
- **Token 管理** — 撤销单个 Token、重生成会话 Token（旧 Token 失效）、切换权限（rw↔ro）
- **跳转终端** — 每会话"连接"按钮，新标签页打开该会话的浏览器终端（Token 预填，服务器密码仍需手填）
- **会话录制 / 录像管理** — 见 [sessions.md](sessions.md)
- **踢出会话** — 断开该 agent 及其所有浏览器并撤销其 Token
- **服务器密码** — 查看当前 `--auth`、在线修改（即时生效）
- **带宽限制** — 设置页可在线修改上下行速率，即时生效并原子写回 `--settings-file`
- **中英文切换** — 右上角切换（自动探测浏览器语言，localStorage 记忆）

### 安全说明

- 后台页面不在公开静态资源目录，无法经 `/admin.html` 等路径访问
- 双层保护：秘密路径（隐藏入口）+ 账户密码登录
- admin session 仅存内存
- 已知局限：撤销 / 重生成 Token 后，若 agent 用 `register_existing`（带缓存 Token 重连）重连，可能重新带回该 Token；不影响在线会话

## HTTP 端点

所有端点统一在 `/agent` 路径下（另有两个页面脚本在根路径）。

### 浏览器 ↔ Relay

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/session/sse` | GET → SSE | 浏览器接收消息流（鉴权：`X-Auth` + Token） |
| `/agent/session/send` | POST | 浏览器发送消息（单条 ≤ 8MB） |
| `/agent/upload` | POST | 文件上传（需服务器密码 + token） |

### Agent ↔ Relay

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/events` | GET → SSE | Agent 接收消息流（`?session=<id>`） |
| `/agent/send` | POST | Agent 发送消息 |
| `/agent/ws/send` | GET → WS | Agent 视频上行（WS） |

### MCP

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/mcp/sse` | GET → SSE | MCP SSE Transport 端点 |
| `/agent/mcp/messages` | POST | MCP JSON-RPC 消息（返回 202 Accepted） |
| `/agent/mcp/put` | PUT | 大文件上传（`?path=`，`X-SR-Token`） |
| `/agent/mcp/get` | GET | 大文件下载（`?path=`，`X-SR-Token`） |

详见 [mcp.md](mcp.md)。

### 桌面

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/desktop/stream` | GET | 桌面视频流（fMP4） |
| `/agent/desktop/ws` | GET → WS | 桌面下行（WS 通道） |

### 网页映射

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/web/ticket` | POST | v1：签发绑定会话的短期凭据（`X-SR-Token` + `X-Auth`） |
| `/agent/web/proxy` | GET | v1：经隧道取远端页面（`?ticket=…&url=…`，返回 `X-SR-Final-URL`） |
| `/agent/web/map` | POST | v2：建（或复用）映射，body `{"url":…}`（仅 rw Token） |
| `/agent/web/m/:map_id` | ANY | v2：透明反代（含 WebSocket 透传） |
| `/sr-web-sw.js` | GET | v2：Service Worker（scope `/`） |
| `/sr-web-shim.js` | GET | v2：注入到映射页面的脚本 |

详见 [web-map.md](web-map.md)。

### 安装与升级

| 路径 | 方法 | 说明 |
|------|------|------|
| `/agent/install` | GET | 一行安装脚本（shell），relay 地址自动注入 |
| `/agent/install.ps1` | GET | 一行安装脚本（PowerShell） |
| `/download/:filename` | GET | 公开制品下载（需 `--download-dir`，支持 `Range`） |
| `/agent/upgrade/blob/:filename` | GET | 鉴权制品下载（rw Token，支持 `Range`） |
| `/agent/resolve` | GET | 代解析 `?host=`（rw Token，仅限 `--upgrade-cdn` 白名单） |

详见 [upgrade.md](upgrade.md)。

### 其他

| 路径 | 方法 | 说明 |
|------|------|------|
| `/api/clock` | GET | relay 时间（`{"epoch_ms": …}`），供浏览器校时 |
| `/`、`/session`、`/style.css`、`/sse.js`、`/term.js`、`/files.js`、`/session.js`、`/webview.js` | GET | 内嵌静态资源 |

### 管理后台 API

全部挂在 `--admin-path` 的值之下（下表以 `<admin>` 代指），除 login 外都要求有效 admin cookie：

| 路径 | 方法 | 说明 |
|------|------|------|
| `<admin>` | GET | 后台页面 |
| `<admin>/login` | POST | 登录 |
| `<admin>/logout` | POST | 登出 |
| `<admin>/api/overview` | GET | 概览数据 |
| `<admin>/api/session/kpi/:sid` | GET | 单会话 KPI |
| `<admin>/api/session/kick` | POST | 踢出会话 |
| `<admin>/api/token/revoke` | POST | 撤销 Token |
| `<admin>/api/token/regenerate` | POST | 重生成会话 Token |
| `<admin>/api/token/permission` | POST | 切换 Token 权限 |
| `<admin>/api/server-auth` | GET / POST | 查看 / 修改服务器密码 |
| `<admin>/api/bandwidth` | GET / POST | 查看 / 修改带宽限制 |
| `<admin>/api/recordings` | GET | 录像文件列表 |
| `<admin>/api/recordings/content` | GET | 录像内容 |
| `<admin>/api/recordings/delete` | DELETE | 删除录像 |
| `<admin>/api/agent/upgrade` | POST | 触发 agent 升级（`{"session_id":…,"force":false}`） |
| `<admin>/api/agent/upgrade/plan` | GET | 只做版本决策与制品定位，不下发指令 |
| `<admin>/api/agent/upgrade/artifacts` | GET | 制品库现状 |

## 带宽限制

`--bandwidth-down-mbps` / `--bandwidth-up-mbps` 限制 relay 整体的出站 / 入站速率，后台"设置 → 带宽限制"可在线修改。

优先级：`--settings-file` 里的同名字段 > CLI 参数 > 不限（0）。后台每次保存都会先写临时文件再 rename 覆盖（原子写）；文件损坏或缺失时按"未配置"处理并退回 CLI 值。

三类流量的处理方式不同：

- **文件传输、升级包、二进制下载** — 等待式限速：只降速、不丢数据
- **桌面视频** — 令牌不足时丢帧并回传 `desktop:congested`。带宽导致的丢帧不计入踢 viewer 的计数，agent 会自动降码率
- **终端与控制等交互消息** — 只记账不等待，保证响应速度
