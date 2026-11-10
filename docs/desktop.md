# 桌面共享

需要 `shell-remote-ui` 二进制（`shell-remote` CLI 版不编译桌面代码，能力上报恒为关闭，浏览器端不显示桌面按钮）。

## 打开与关闭

会话页工具栏点"桌面"开流（默认关闭，点击才开），收到 `desktop:started` 后浏览器自动连接视频流；再点"终端"切回。

- **权限**：开流 / 关流需 rw Token；观看桌面画面 rw / ro 均可
- **传输**：fMP4 分片，浏览器用 WebCodecs（安全上下文下可用）或 MSE 播放；中途加入的 viewer 从最近的关键帧（IDR）开始

## CLI 示例

```bash
./shell-remote-ui agent --relay-url https://relay.example.com \
  --desktop-capture auto --desktop-fps 30
```

常用组合：

```bash
./shell-remote-ui agent --relay-url https://relay.example.com --relay-insecure \
  --desktop-capture x11 --desktop-codec av1 --desktop-fps 30 \
  --desktop-quality balanced --desktop-max-bitrate 2000 --desktop-lan-port 39001
```

## 参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--desktop-capture` | `auto` | 捕获后端：`auto` / `dxgi` / `gdi` / `x11` / `wayland` / `none`。`none` 完全关闭桌面共享 |
| `--desktop-codec` | `av1` | 编码格式：`av1`（libaom）/ `h264`（openh264）。初始化失败自动按 av1→h264 回退 |
| `--desktop-fps` | `30` | 编码帧率上限。QoS 内容驱动：静止约 1fps，动态满帧 |
| `--desktop-capture-fps` | `0` | 抓帧独立上限（fps），与编码帧率解耦。`0` = 不限（动态时全速抓帧，由编码 `min_gap` 跳帧兜底）。设值则动态桌面也按此节流，省 X/DXGI 往返 |
| `--desktop-max-bitrate` | `0`（自动） | 最大编码码率（kbps）。`0` = 按 rustdesk 模型 `base_bitrate(分辨率) × 质量档`，1080p balanced ≈1388kbps；显式设值即硬顶 |
| `--desktop-min-bitrate` | `80` | 最小编码码率（kbps）。静态桌面足够，动态由 ABR 拉回 |
| `--desktop-quality` | `balanced` | 质量档：`speed`（0.5）/ `balanced`（0.67）/ `best`（1.5），决定目标码率 |
| `--desktop-display` | 平台默认 | 显示器**枚举序号**（`"0"` = 第 1 块、`"1"` = 第 2 块，与 `desktop:started.displays` 数组下标一致；Windows 为 DXGI/GDI 枚举顺序，X11 为 RANDR 输出顺序）。非数字值在 X11 上仍按旧语义当作 X display 字符串（如 `:1`，默认取 `$DISPLAY`），且**序号选屏时也复用它做 X 连接串**——浏览器切屏不会覆盖它 |
| `--desktop-lan-port` | `0` | LAN 直连监听端口。`0` = 不启用；非 0 时在该端口起本地 HTTP 端点供同网段浏览器拉流（需带会话 token，见下文"下行通道"） |
| `--view` | `auto` | 状态面板视图：`auto` / `window` / `tui` / `headless`。`auto` = 有 `DISPLAY` 或 Windows 开原生窗口，否则有 TTY 开终端面板，都没有则 headless |

## 编码

**纯软件编码**，不调用 GPU。Windows 上 DXGI 只用于屏幕捕获，编码全在 CPU；D3D11 设备在无可用 GPU 驱动时自动回退 WARP 软光栅，带显卡与无显卡环境行为一致。硬件编码（VAAPI / Media Foundation）尚未实现。

- **AV1（默认）**：libaom。构建产物需链接静态 libaom（见 [build.md](build.md) 的 `tools/build-libaom.sh`），未链接时能力上报不含 `codec:av1`
- **H.264**：OpenH264，源码内联编译
- 运行时可在会话页切换 codec（`desktop:codec` 消息），agent 重建桌面流
- 浏览器侧若 30s 窗口内解码器报错 ≥3 次，会判定该 codec 不可解并请求 agent 切到下一个可用编码

## 捕获

| 平台 | 后端 |
|------|------|
| Windows | DXGI Desktop Duplication（默认，多屏可选）→ GDI BitBlt 回退（多屏可选） |
| Linux | X11（含 XWayland）；Wayland 原生需 `--features wayland` 构建，运行时依赖 xdg-desktop-portal + PipeWire |

Wayland 支持不进默认构建（`wayland` feature 会引入 zbus / gstreamer），未启用时 `--desktop-capture wayland` 给出可操作的错误。

## 灰度模式

会话页桌面控制栏的"灰度"开关，弱网下省带宽：

- **AV1**：切到 libaom 原生 monochrome 码流，色度平面不进码流（单测 `test_av1_monochrome_saves_bytes_and_flags` 断言同内容下省 ≥20% 字节），重建流后生效
- **H.264 / 其他**：编码前把 I420 的 UV 平面置中性 128，下一帧即时生效

## 多显示器

`desktop:started` 上报远端显示器拓扑（X11 RANDR 输出 / Windows DXGI 输出 / GDI 显示器枚举），指标面板"远端显示器"行可见。多屏时会话页出现"显示器"下拉，选择后重建桌面流切换到对应屏幕。

选屏协议统一为**枚举序号**，三个平台语义一致：下拉的 value 是 `desktop:started.displays` 的**数组下标**，`"0"` 是第一块、`"1"` 是第二块。

- **Windows**：DXGI 与 GDI 的拓扑上报和选屏走同一份枚举顺序（同一个 `enumerate_outputs` / `list_monitors_gdi`），序号不会错位
- **X11**：序号 = RANDR 输出顺序；选中后 X 连接仍走 `$DISPLAY`，采集区域裁剪到该输出的 CRTC 矩形（`GetImage`/`ShmGetImage`/XComposite 全部按 `x,y,w,h` 取），鼠标注入加上该 CRTC 原点
- 未连接/无 CRTC 的输出仍出现在 `displays` 里（尺寸 0），浏览器过滤掉不提供选择；直接按序号选它会拒绝并报错（不会悄悄回落到整屏）
- 鼠标落点：浏览器上报的坐标是**采集画面内**的，agent 注入前加上所选显示器在虚拟桌面中的原点（如第二屏 `+1280+0`），否则点击会落到主屏
- 显示器在系统设置里被挪动/换分辨率时，采集区域与注入原点会自动跟随（X11 走 RANDR 事件、GDI/DXGI 每拍重读）

## 下行通道：LAN → P2P → relay

默认探测顺序全部自动，用户无感；指标面板"下行通道"行显示当前路径（`lan` / `p2p` / `relay`）。

1. **LAN** — agent 带 `--desktop-lan-port` 时，浏览器依次探测 `http://<agent-ip>:<port>/agent/desktop/stream?token=<会话 token>`（单地址探测上限 9s，失败即换下一条）。该端点绑 `0.0.0.0`，同网段任何主机都能访问，因此**必须鉴权**：请求要带本会话当前有效的 token（rw / ro 均可，浏览器用查询参数 `?token=` 携带，避免自定义头触发 CORS 预检；`Authorization: Bearer <token>` 亦支持）。缺失或不匹配返回 `401`（带 CORS 头便于浏览器读取状态码），且未授权请求不占用 viewer 槽位。token 刷新后旧 token 立即失效，LAN 通道自然回退 P2P / relay。**已知限制**：该拉流是 `http://` 混合内容，当 relay / 会话页以 HTTPS 提供时浏览器会直接拦截此 fetch——此时 LAN 通道不可用，无挂起、快速回退 P2P/relay，仅在纯 HTTP 部署下生效
2. **WebRTC** — 协商经 relay 信令（`desktop:p2p-*`）完成，DataChannel 承载 fMP4 字节（不可靠模式、丢旧保新）
3. **relay** — 前两者失败时回退到 `/agent/desktop/stream` 转发，功能不劣于只有 relay 的旧版

P2P 已知限制：

- 高动态持续帧率受 str0m SCTP cwnd 慢启动限制（LAN / relay 无此问题）
- P2P 会话为单活跃 viewer，并发 viewer 自动走 relay
- 被 P2P 抢占踢出后，本连接周期内不再尝试 P2P（防止双方互踢）

## 状态面板

`--view auto` 时按环境自动选择：

| 视图 | 触发条件 | 内容 |
|------|----------|------|
| `window` | 有 `DISPLAY`（Linux 桌面）或 Windows | 原生窗口（egui/eframe），状态 / 会话 id / token / relay 地址 / 延迟 / 已运行时长，"刷新 token"与"退出"按钮 |
| `tui` | 无桌面但有 TTY | 终端状态面板（ratatui），内容同上，`r` 刷新 token、`q` / `Esc` / `Ctrl-C` 干净退出 |
| `headless` | 无 `DISPLAY` 且无 TTY（systemd / nohup / 重定向） | 不渲染界面，行为与普通 agent 一致 |

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

- **刷新 token**：重新注册换取新 Token，**会话 id 保持不变**，旧 Token 立即失效。仅在非 `--key`（随机 Token）模式下值才会变；`--key` 模式下 Token 即 key，固定不变
- **退出**：还原终端 → 请求 agent 停机，PTY 子进程由 drop 回收，不残留孤儿进程
- 交互视图下业务日志默认落 `~/.shell-remote/`，避免刷卡刷屏污染界面；已设 `SR_LOG_DIR` 则尊重其值

窗口 GUI 走 winit X11（纯 Rust 协议 x11rb），libxkbcommon / libGL 运行时 dlopen，二进制保持 musl 全静态；中文显示自动发现系统 CJK 字体（Windows 微软雅黑 / Linux Noto CJK / WQY 等）。
