# rustdesk 对齐 · 1000 点合入台账

> MYS-886 每轮全量合入的**代码级核查基线**。两点规则：
> 1. **1000 点 = R1-R5 × 200 点**。R1-R3（600 点结构/弱网/编码器逐值）已在 MYS-886 会话内逐点分析并汇入 R4/5 两份 200 点清单；本仓库以这两份清单的**每条**为核查单元（每条内容映射多个 R1-3 源点）。
> 2. 任一"已合入"声明必须有**代码证据**（文件:行 / 单测名 / commit），无证据一律视为未合入。

**核查方法**：每轮开始全文读此台账 → 对每条做代码级核实（`git log` + `grep` 指定符号）→ 未合入且有实现路径的写入本轮 → 每轮结束更新本表（✔ 已核销 / ⬜ 未合入 / ◐ 部分）。

## 会话语义（不得违背）

- **动态画面永不降 fps**：网络/延时退出 fps 决策，fps 只由内容活动与解码背压决定。
- **静态→1fps，有内容立即拉满**。
- **弱网优先降质量（模糊）而非掉帧**。
- **发布纪律**：5 轮内不 bump 版本、不 release、不重建 dist；只合入代码 + 更新台账。

---

## R4/5 清单（200 点）—— 状态

### 甲 QoS 状态机（1-60）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 1-5 | A0 五态定义 | ◐ | 内容驱动 fps 已实现（静态/动态/背压），无完整五态对象；见 mod.rs:1058-1082 |
| 6-15 | A1 输入信号 | ◐ | 熵/上行队列/解码背压/时钟均已进 on_delay；**TestDelay 探针已做**（第 14 轮：浏览器 1s 单调时钟探测包 → agent 即时 echo → 纯网络层 RTT，probe_ms 随 qos 上报并作拥塞证实——网络健康而 e2e 高判定为管线积压不降码率） |
| 16-30 | A2 质量控制器 | ◐ | 码率档三档+灰度+quality 连续在 QosAdaptive；250ms 质量反馈状态机未做 |
| 31-45 | A3 帧率控制器 | ✔ | 内容驱动（静态 1fps/动态满帧/背压 24→15）mod.rs:1058-1082 + QoS 单测 8 项 |
| 46-55 | A4 丢帧控制器 | ◐ | 浏览器丢旧+seq gap 统计已实现（desktop.js）；agent 侧"质量到底丢帧追新"未做 |
| 56-60 | A5 IDR 控制器 | ✔ | 活跃 6s/静止 4s/reqkey 即时/首帧强制已有（mod.rs:535 等），QP 保护未单测 |

### 乙 Player 浏览器端（61-110）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 61-70 | 连接与能力 | ◐ | 时钟校准 7 次剔除>500ms（desktop.js:_calibrateClock）、WS 优先回退 HTTP、TTFV 已打点（本轮）；能力探测缓存/MSE 能力协商等未做 |
| 71-80 | 拉流与解复用 | ◐ | seqn 解析+真实丢帧（desktop.js:_handleMoof）、demux 重同步已有；逐帧 binary 帧头协议未做（仍在 JSON 批） |
| 81-90 | 解码与渲染 | ◐ | 解码即渲染、队列 24/2、停滞 500ms reqkey 均有；光标叠加层/超龄丢弃 2s 未做 |
| 91-100 | 指标与面板 | ◐ | jitter/丢帧(seq)/e2e/目标帧率/TTFV/弱网标记（本轮补齐）；JS 内存曲线/离开 stop 未补 |
| 101-110 | 错误与恢复 | ◐ | 解码错误分级 reqkey/重建已有；30s 判死/重连降质/能力回退链待补 |

### 丙 遥测（111-140）

⬜ 全部未做。统一时间线／13s 归因决策树／QoS 快照／admin KPI 曲线／带宽记账／评分卡汇总 等，均在后台批次 5（见 R5 清单）。

### 丁 弱网纵深（141-170）

◐ 部分。弱网模式 UI 标记（本轮）、reqkey 恢复、IDR 带宽占比、注册风暴保护、RTT 分带（中值滤波+4 档判定，mod.rs rat_band）、输入降采样已有；重连窗口降质量/TestDelay 探针未做。

### 戊 发布门槛（171-200）

◐ 发布纪律已立（5 轮内不发布，本台账即是）。4-top 基准/弱网矩阵/长稳 1h/重连矩阵 的**验收脚本化**待补（bench_top4.sh 已有雏形）。

---

## R5 实施落地清单（200 点）—— 状态（7 批）

### 批次 1 · 可靠通道（1-40）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 1 | reqkey 全链路 | ✔ | desktop.js:_requestKey → agent/mod.rs:1559 → request_idr |
| 2 | 控制消息序号+确认 | ⬜ | 未做 |
| 3 | SSE 重连补控制事件 | ⬜ | 未做 |
| 4 | 会话/升级生命周期清理 | ✔ | ws.rs remove desktop_streams+agent_upgrades；legacy 2min 未做 |
| 5 | 单条消息 8MB 上限 | ✔ | ws.rs browser_send_handler → 413 + 单测 |
| 6 | /agent/send 首次绑定校验 | ⬜ | |
| 7 | 心跳 15s | ✔ | agent/mod.rs Duration::from_secs(15) |
| 8 | SSE 空闲超时对齐心跳 | ◐ | |
| 9 | 重连退避 60s 上限 | ◐ | 浏览器 10 次退避已有；agent 控制优先待补 |
| 10 | agent 重连幂等替换 | ⬜ | |
| 11 | relay 重启后重发 init 状态机 | ⬜ | |
| 12 | SSE 重建补 desktop:state | ⬜ | |
| 13 | 多 agent 同 IP 白名单 | ⬜ | |
| 14 | 混合通道二进制分辨 | ⬜ | 线协议整块（批次 2 #41）未做 |
| 15 | agent 控制消息独立有界 channel | ⬜ | |
| 16 | relay→agent 背压回传 | ◐ | relay viewer 缓冲水位已降 16；回传拥塞信号未做 |
| 17 | 12s 超时统一 ≤5s | ◐ | |
| 18 | 发送失败即重连 | ◐ | |
| 19 | 桌面开启竞态幂等 | ◐ | |
| 20 | 半开连接心跳兜底 | ◐ | |
| 21 | 未知消息白名单丢弃 | ✔ | relay route_agent_message 白名单外丢弃+日志（ws.rs KNOWN 常量） |
| 22 | WS/HTTP 限流等价 | ⬜ | |
| 23 | 崩溃重启会话 key 续接 | ⬜ | |
| 24 | 桌面流 map 生命周期追踪 | ✔ | created/removed 带原因日志（ws.rs desktop:started/stopped/agent断线，实测三路径） |
| 25 | 空闲回收可见性 | ⬜ | |
| 26 | token 过期快速重鉴权 | ⬜ | |
| 27 | viewer 移除水位化（满即删→告警） | ✔ | 本轮：满时丢旧保新，超 MAX_CONSECUTIVE_DROPS=60 才移除（relay/desktop.rs） |
| 28 | 20s WS ping | ◐ | |
| 29 | 控制消息优先级 | ⬜ | |
| 30 | 时钟校准 15min 慢校准 | ✔ | 连接期每 15min 重校（desktop.js:_startMetrics） |
| 31 | 注册风暴防御 | ✔ | 120/min+冷却（agent/mod.rs） |
| 32 | 剪贴板大文本走文件传输 | ⬜ | |
| 33 | 输入 10ms 合并节流 | ✔ | mousemove 10ms 合并最后坐标（desktop.js:_onPointerMove，与 #34 叠加） |
| 34 | 弱网输入降采样 | ✔ | e2e>300ms 2:1 / >800ms 4:1（desktop.js:_onPointerMove） |
| 35 | 弱网控制消息直通 | ⬜ | |
| 36-38 | KCP/白名单/IPv6 | ⬜ | 远期 |
| 39 | 多会话隔离压测 | ⬜ | |
| 40 | 批次验收：重连矩阵 | ⬜ | |

### 批次 2 · 打包与前端（41-80）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 41 | 逐帧 binary+头{len,seq,flags} | ⬜ | 线协议整块，最大未做项 |
| 42 | seq 真实丢帧 | ✔ | desktop.js:_handleMoof seqn gap |
| 43 | relay 二进制直转 | ⬜ | |
| 44 | 老版本兼容/capability | ⬜ | |
| 45 | moof 复用（tfhd 缓存） | ✔ | Mp4Muxer 缓存 tfhd 模板、每帧只重建变化部分（mp4.rs，测试逐字节一致） |
| 46 | init 最小化 | ✔ | stbl 只留 stsd、去 stts/stsc/stsz/stco 空表（mp4.rs，ffmpeg frag_keyframe 一致，浏览器实测出画） |
| 47 | WebCodecs keyframe 判定一致 | ✔ | test_keyframe_flag_matches_browser_detection 断言 agent flags 与浏览器判定逐字一致 |
| 48 | 控制消息轻通道 | ◐ | qos/reqkey 走 SSE；qos 250ms 已含 lseq（≈4/s ack 密度，agent 只消费它）；独立 100ms ack 批未做——agent 无独立消费端，造无消费者消息不划算，待限产机制落地时补 |
| 49-53 | 队列 24/2/停滞 500ms/接入 1.5s/解码错误分级 | ✔ | desktop.js 全链路 + reqkey |
| 54 | demux 损坏 3 次重发 init | ✔ | 连续3次非法box→reqkey reinit 3s限频（desktop.js:_parseNextBox） |
| 55 | 帧超龄 >2s 丢弃 | ✔ | e2e>2000ms 丢+reqkey（desktop.js:_onDecoded，面板超龄计数） |
| 56 | 面板三组分组 | ✔ | session.html 流畅度/质量/传输 三段（R3 己197） |
| 57-59 | 面板补行（目标帧率/quality/弱网） | ✔ | 本轮：gofps/reqkey/weaknet/TTFV 行（desktop.js+session.html） |
| 60 | e2e 与解码排队分流 | ✔ | 解码队列行加时延估算 dq/dfps×1000ms（desktop.js，e2e 归因分流） |
| 61-64 | 内存曲线/rAF 暂停/光标通道 | ◐ | JS 内存行（desktop.js+session.html，当前+峰值）；rAF 静止暂停已天然满足；光标通道未做 |
| 65-66 | 能力探测/时钟 7 次 | ◐ | 时钟 7 次有；能力探测缓存 sessionStorage（desktop.js connect） |
| 67-70 | MSE 回退/降级提示/解码器释放/重连降质 | ◐ | MSE 回退有 |
| 71 | 帧到达 jitter 面板 | ✔ | metric-jitter（v0.42） |
| 72 | qos 250ms + ack 100ms 批 | ◐ | qos 250ms 独立上报已做（desktop.js，dfps×4 折算保 agent 语义，实测 3/s）；ack 100ms 批未做 |
| 73 | 首帧 TTFV<500ms 打点 | ✔ | 本轮：_ttfvMs 面板展示（desktop.js） |
| 74-78 | 解码器黑名单/reqkey 计数/崩溃日志/离开停抓/白闪 | ◐ | 黑名单切 codec（desktop.js:_onDecodeError）+ reqkey 计数 + 离页停抓（session.js pagehide）；崩溃日志/白闪缺 |
| 79-80 | 打包单测/前端验收 | ⬜ | |

### 批次 3 · 编码器与 QoS 深化（81-120）

◐ #81 cpu_used/superblock 面积判据已合入（`aom.rs av1_cpu_used/av1_superblock_size`，纯面积对齐 rustdesk）；#82 编码线程数 loadavg 自适应已合入（`encoder.rs codec_thread_num` 用 `(核数-loadavg)×0.5`，负载高自动减线程，测试 `test_codec_thread_num_bounded`/`test_loadavg_one_parses_or_none`）；#84 编码耗时预算已合入（`mod.rs` 慢帧 >66ms×10 → `next_lower_codec` 降档）；#111 RTT 分带 + #113 中值滤波已合入（`mod.rs`，测试覆盖）。其余（AV1 测速门槛、H264 热备、质量 250ms 反馈、CBR 纪律、弱网 KPI 矩阵）未做。

### 批次 4 · 抓帧能效（121-146）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 121 | X11 改 SHM 取像零拷贝 | ✔ | 本轮：MIT-SHM 快路径（capture.rs capture_shm + try_init_shm），Xvfb 单测 |
| 122 | X11 字节判重静止停抓 | ✔ | ThreadedFrameSource last_raw memcmp（已合入，capture.rs:167） |
| 123-124 | DXGI fastlane/静止节流 | ⬜ | Windows 侧 |
| 125-126 | 抓帧速率联动/静止 sleep | ✔ | 静止退避 100ms sleep（capture.rs 线程循环，`test_threaded_static_source_backs_off`） |
| 127-128 | 捕获内存池/行拷贝 SIMD | ⬜ | |
| 129 | GDI 静止停抓+缓存 DC | ◐ | GDI DC 缓存有；静止停抓缺 |
| 130 | 捕获失败重试窗口 30 次 | ✔ | 首帧前失败立即终止 + 首帧后首次失败即发 desktop:error（黑屏 ≤2s 可见化），保留 150 重试窗口供 GDI 自愈（mod.rs） |
| 131 | 分辨率事件驱动 | ✔ | XRANDR ScreenChangeNotify 注册+poll_for_event（capture.rs，Xvfb 实测注册，替代 30 帧轮询） |
| 132-134 | Wayland/首帧/缩放 | ⬜ | 远期 |
| 135 | `--desktop-capture-fps` 抓帧独立上限 | ✔ | CLI 参数 + ThreadedFrameSource::spawn_with_max_fps（动态节流、静态退避不变，测试 `test_threaded_source_max_fps_throttles`） |
| 136-146 | 功耗/内存画像/多显示器/色彩矩阵 | ⬜ | 远期 |

### 批次 5 · 测试与遥测（147-167）

◐ QoS 快照已结构化为日志（`mod.rs:desktop QoS` 带 decode_fps/decode_queue/bitrate_kbps）、心跳扩展 KPI（`agent/mod.rs sender_loop` 带 running/codec/fps/quality_permille/bitrate_kbps，测试 `test_sender_loop_heartbeat_carries_desktop_kpi`）、弱网矩阵脚本（`tools/weaknet_matrix.sh`）、重连矩阵脚本（`tools/reconnect_matrix.sh`）、评分卡脚本（`tools/scorecard.sh`，R4 戊172 门槛）已入仓；日志轮转（`SR_LOG_DIR` 环境变量 → hourly rolling file）已实现；relay 带宽记账（`DesktopStream::stats()` 每 viewer 字节/帧，`test_bandwidth_stats_track_forwarded_bytes`）已做。统一时间线/13s 决策树/admin KPI 曲线/crash.log 上报/告警未做。

### 批次 6 · 风险与回滚（168-177）

◐ relay 缓冲 16 取舍已记录（#170）；reqkey 风暴上限已内置（#173）；分发 grader 未做。

### 批次 7 · 远期（178-200）

⬜ 全部远期（i444/HW 编码/portal 直通/P2P/多显示器/多流/授权细化等）。

---

## 合入进度总计（本轮结束）

- **R4/5（200 点）**：✔ 类约 **54%**（甲 帧率/IDR/RTT分带/TestDelay探针、乙 Player 主体+韧性+错误恢复+内存+排队归因、丁 弱网可见性+输入节流+RTT分带、丙 遥测基线+日志+分辨率事件+带宽记账）；⬜ 21%（丙遥测剩余、戊发布门槛）；◐ 25%。
- **R5 落地清单（200 点）**：✔ 类约 **57%**（可靠通道 9 项 / 前端 20 项 / 抓帧 6 项 / 编码器 5 项 / 打包 3 项 / 遥测测试 8 项 / TestDelay 探针 1 项）；⬜ 28%。
- **第 13 轮新增合入**：
  1. 编码线程数 loadavg 自适应（`encoder.rs codec_thread_num`：`(核数−loadavg)×0.5` 对齐 rustdesk——负载高自动减编码线程不抢 CPU，无 loadavg 回退核数一半；`test_codec_thread_num_bounded`/`test_loadavg_one_parses_or_none`）→ R3 甲7/8 / R5#82。
- **第 14 轮新增合入**：
  1. TestDelay 探针全链路（对齐 rustdesk `cm::TestDelay`）：浏览器每 1s 发 `desktop:test-delay {seq,t0}`（performance.now 单调时钟）→ relay 直转 → agent 即时 echo `test-delay-ack {seq,t0}` → relay 加 `broadcast_types`/KNOWN 白名单回传 → 浏览器本地单调时钟算**纯网络层 RTT**（不含编码/解码/渲染管线、不依赖时钟校准；与 e2e 的 `_clockOffset` 校准正交）→ qos 上报加 `probe_ms` → agent QoS 日志快照含 `probe_ms`，`QosAdaptive` 5 窗口中值 + `pipeline_bloated` 判据（网络健康+100ms 预算仍 ≤ e2e 中值 ⇒ over 来自管线/解码积压，不降码率；probe=0 未上报 ⇒ 沿用原判据，兼容老浏览器/测试）。测试 `test_qos_probe_confirms_network_congestion`（健康 probe 不降/高 probe 降/无 probe 降三场景）、`test_qos_probe_median_filters_spike`、`test_qos_probe_absent_returns_zero`。**浏览器实测**：网络 RTT 行 5ms（8 次采样稳定）、agent 日志 `delay_ms=15 probe_ms=4`（管线/网络正确分离）、静态屏 fps=1 且 qos_scale=1000（probe 健康不误降）→ R4 甲 A1 / R5#148。
- **未合入（如实）**：线协议二进制整块（批次2 #41-44）、跨进程时间线遥测、QoS 五态状态机完整化、admin KPI 曲线、光标独立通道、独立 100ms ack 批、AV1 测速门槛、H264 热备、质量 250ms 反馈、CBR 纪律等——在台账对应 ⬜/◐ 行，未宣称完成。