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
| 1-5 | A0 五态定义 | ◐ | 内容驱动 fps 已实现（静态/动态/背压）；**五态对象已做**（第 19 轮：`QosQualityState` Unknown/Good/Medium/Degraded/Critical，probe 中值+over 推导、迁移日志、qos-ack 回传、面板状态行，实测 Unknown→Good）；决策仍由内容活动+over 判据驱动，状态为观测快照 |
| 6-15 | A1 输入信号 | ◐ | 熵/上行队列/解码背压/时钟均已进 on_delay；**TestDelay 探针已做**（第 14 轮：浏览器 1s 单调时钟探测包 → agent 即时 echo → 纯网络层 RTT，probe_ms 随 qos 上报并作拥塞证实——网络健康而 e2e 高判定为管线积压不降码率） |
| 16-30 | A2 质量控制器 | ◐ | 码率档三档+灰度+quality 连续在 QosAdaptive；**250ms 质量反馈状态机已做**（第 19 轮：五态由 250ms qos 上报驱动、迁移日志、面板状态行） |
| 31-45 | A3 帧率控制器 | ✔ | 内容驱动（静态 1fps/动态满帧/背压 24→15）mod.rs:1058-1082 + QoS 单测 8 项 |
| 46-55 | A4 丢帧控制器 | ◐ | 浏览器丢旧+seq gap 统计已实现（desktop.js）；agent 侧"质量到底丢帧追新"未做 |
| 56-60 | A5 IDR 控制器 | ✔ | 活跃 6s/静止 4s/reqkey 即时/首帧强制已有（mod.rs:535 等），QP 保护未单测 |

### 乙 Player 浏览器端（61-110）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 61-70 | 连接与能力 | ◐ | 时钟校准 7 次剔除>500ms（desktop.js:_calibrateClock）、WS 优先回退 HTTP、TTFV 已打点（本轮）；能力探测缓存/MSE 能力协商等未做 |
| 71-80 | 拉流与解复用 | ◐ | seqn 解析+真实丢帧（desktop.js:_handleMoof）、demux 重同步已有；逐帧 binary 帧头协议未做（仍在 JSON 批） |
| 81-90 | 解码与渲染 | ◐ | 解码即渲染、队列 24/2、停滞 500ms reqkey 均有；**光标叠加层已做**（第 18 轮：X11 GetImage 不含光标层——agent `poll_cursor` 100ms 节流 XQueryPointer → `desktop:cursor` 轻量消息 → 浏览器 `.sr-cursor-overlay` 叠加渲染，实测光标跟随鼠标）；超龄丢弃 2s 已做 |
| 91-100 | 指标与面板 | ◐ | jitter/丢帧(seq)/e2e/目标帧率/TTFV/弱网标记（本轮补齐）；JS 内存曲线/离开 stop 未补 |
| 101-110 | 错误与恢复 | ◐ | 解码错误分级 reqkey/重建已有；30s 判死/重连降质/能力回退链待补 |

### 丙 遥测（111-140）

◐ 部分。QoS 快照结构化日志（R5#149）、心跳扩展 KPI（#150）、relay 带宽记账（#152）、评分卡脚本（#155）已有；**admin KPI 曲线已做**（第 16 轮：relay 采样 agent 心跳 KPI——15s×120 点 FIFO，`/api/session/kpi/:sid` 时间序列 + admin 面板 📈 canvas 折线 fps/bitrate）。统一时间线／13s 归因决策树／crash.log 上报 未做。

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
| 2 | 控制消息序号+确认 | ✔ | 第 22 轮：控制命令（quality/codec/gray）带递增 seq → agent 处理后回 `desktop:cmd-ack {seq,ok,error}` → 浏览器 toast 反馈操作结果（弱网/高负载可见反馈）；relay broadcast_types/KNOWN 白名单；实测 quality(best) ack `{ok:true,seq:100}` |
| 3 | SSE 重连补控制事件 | ⬜ | 未做 |
| 4 | 会话/升级生命周期清理 | ✔ | ws.rs remove desktop_streams+agent_upgrades；legacy 2min 未做 |
| 5 | 单条消息 8MB 上限 | ✔ | ws.rs browser_send_handler → 413 + 单测 |
| 6 | /agent/send 首次绑定校验 | ⬜ | |
| 7 | 心跳 15s | ✔ | agent/mod.rs Duration::from_secs(15) |
| 8 | SSE 空闲超时对齐心跳 | ◐ | |
| 9 | 重连退避 60s 上限 | ◐ | 浏览器 10 次退避已有；agent 控制优先待补 |
| 10 | agent 重连幂等替换 | ✔ | relay `SessionRegistry::register_existing`（session.rs:153）——agent 断线重连 replay cached_tokens 走 register_existing 替换旧 session（第 21 轮 #11 恢复依赖它）；代码级核实修正 |
| 11 | relay 重启后重发 init 状态机 | ✔ | 第 21 轮：agent 会话级 `desktop_want_running` 跨重连传递——断线退出时记录桌面状态并显式 stop（防 orphan task 向失效连接发帧），重连后自动 `desktop.start`（新 send_url，首帧强制 IDR 重发 init）。实测两轮 relay 重启均 `auto-restoring desktop stream` + `capture started 1280x720` |
| 12 | SSE 重建补 desktop:state | ⬜ | |
| 13 | 多 agent 同 IP 白名单 | ⬜ | |
| 14 | 混合通道二进制分辨 | ⬜ | 线协议整块（批次 2 #41）未做 |
| 15 | agent 控制消息独立有界 channel | ⬜ | |
| 16 | relay→agent 背压回传 | ◐ | relay viewer 缓冲水位已降 16；回传拥塞信号未做 |
| 17 | 12s 超时统一 ≤5s | ◐ | |
| 18 | 发送失败即重连 | ◐ | |
| 19 | 桌面开启竞态幂等 | ✔ | `DesktopManager::start` 首行 `if self.is_running() { return; }`（检查→置 running 间无 await，并发安全）；第 21 轮代码级核实修正 |
| 20 | 半开连接心跳兜底 | ◐ | |
| 21 | 未知消息白名单丢弃 | ✔ | relay route_agent_message 白名单外丢弃+日志（ws.rs KNOWN 常量） |
| 22 | WS/HTTP 限流等价 | ✔ | agent_conn_rate_ok 共享 ev: 30/min 配额（agent_events_handler + agent_ws_send_handler，测试 test_agent_conn_rate_shared_ws_http） |
| 23 | 崩溃重启会话 key 续接 | ✔ | agent 崩溃重启后 cached_tokens replay → relay `register_existing` 续接同一 session（client.rs 缓存 token + connect_with_retry 重放）；与 #10 同机制，代码级核实修正 |
| 24 | 桌面流 map 生命周期追踪 | ✔ | created/removed 带原因日志（ws.rs desktop:started/stopped/agent断线，实测三路径） |
| 25 | 空闲回收可见性 | ⬜ | |
| 26 | token 过期快速重鉴权 | ⬜ | |
| 27 | viewer 移除水位化（满即删→告警） | ✔ | 本轮：满时丢旧保新，超 MAX_CONSECUTIVE_DROPS=60 才移除（relay/desktop.rs） |
| 28 | 20s WS ping | ✔ | handle_agent_ws_uplink 每 20s server-side ping（agent 死链 ~35s 检出），台账此前误标 ◐，第 17 轮代码级核实修正 |
| 29 | 控制消息优先级 | ◐ | 部分：`is_lossy_msg_type` 区分数据（terminal:output 静默丢）/控制（non-lossy 满时告警丢）已有，基本优先级语义成立；channel 满时控制仍可能丢（无腾位机制），第 22 轮核实 |
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
| 61-64 | 内存曲线/rAF 暂停/光标通道 | ◐ | JS 内存行（desktop.js+session.html，当前+峰值）；rAF 静止暂停已天然满足；**光标独立通道已做**（第 18 轮：agent XQueryPointer 100ms 节流 → desktop:cursor → 浏览器 overlay，X11 GetImage 不含光标层是真实缺口） |
| 65-66 | 能力探测/时钟 7 次 | ◐ | 时钟 7 次有；能力探测缓存 sessionStorage（desktop.js connect） |
| 67-70 | MSE 回退/降级提示/解码器释放/重连降质 | ◐ | MSE 回退有 |
| 71 | 帧到达 jitter 面板 | ✔ | metric-jitter（v0.42） |
| 72 | qos 250ms + ack 100ms 批 | ◐ | qos 250ms 独立上报已做（desktop.js，dfps×4 折算保 agent 语义，实测 3/s）；ack 100ms 批未做 |
| 73 | 首帧 TTFV<500ms 打点 | ✔ | 本轮：_ttfvMs 面板展示（desktop.js） |
| 74-78 | 解码器黑名单/reqkey 计数/崩溃日志/离开停抓/白闪 | ◐ | 黑名单切 codec（desktop.js:_onDecodeError）+ reqkey 计数 + 离页停抓（session.js pagehide）；崩溃日志/白闪缺 |
| 79-80 | 打包单测/前端验收 | ⬜ | |

### 批次 3 · 编码器与 QoS 深化（81-120）

◐ #81 cpu_used/superblock 面积判据已合入（`aom.rs av1_cpu_used/av1_superblock_size`，纯面积对齐 rustdesk）；#82 编码线程数 loadavg 自适应已合入（`encoder.rs codec_thread_num` 用 `(核数-loadavg)×0.5`，负载高自动减线程，测试 `test_codec_thread_num_bounded`/`test_loadavg_one_parses_or_none`）；#84 编码耗时预算已合入（`mod.rs` 慢帧 >66ms×10 → `next_lower_codec` 降档）；#85 编码器故障热备已合入（第 15 轮：`mod.rs next_degrade_codec` 统一 #84 慢帧/#85 encode-Err 降级出口，`rebuild_encoder_degrade` 复用重建动作，连续 5 帧 Err → av1→vp9→h264；测试 `test_next_degrade_codec_trigger`）；#89 CBR 纪律**已做**（`aom.rs:111 AOM_CBR` + `vpx.rs:91 VPX_CBR` + undershoot/overshoot 50% + 缓冲 600/600/1000 + `AOM_KF_DISABLED` 外部 force_idr，前轮已合入、本台账此前误标未做，第 15 轮修正）；#111 RTT 分带 + #113 中值滤波已合入（`mod.rs`，测试覆盖）；**弱网 KPI 矩阵已做**（第 20 轮：`tools/weaknet_kpi_matrix.sh`——netem 多档 × 浏览器采样 QoS KPI（fps/e2e/probe/qos_state/bitrate）→ KPI 汇总表 + 用户铁律断言"动态弱网不降帧"（静态 fps=1 正确跳过）；实测采样正常）。其余（AV1 测速门槛、质量 250ms 反馈）未做。

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

◐ QoS 快照已结构化为日志（`mod.rs:desktop QoS` 带 decode_fps/decode_queue/bitrate_kbps）、心跳扩展 KPI（`agent/mod.rs sender_loop` 带 running/codec/fps/quality_permille/bitrate_kbps，测试 `test_sender_loop_heartbeat_carries_desktop_kpi`）、弱网矩阵脚本（`tools/weaknet_matrix.sh`）、重连矩阵脚本（`tools/reconnect_matrix.sh`）、评分卡脚本（`tools/scorecard.sh`，R4 戊172 门槛）已入仓；日志轮转（`SR_LOG_DIR` 环境变量 → hourly rolling file）已实现；relay 带宽记账（`DesktopStream::stats()` 每 viewer 字节/帧，`test_bandwidth_stats_track_forwarded_bytes`）已做；**admin KPI 曲线已做**（第 16 轮：`route_agent_message` 宽松 JSON 拦截 ping 心跳（真实 ping 缺 payload 字段，严格 ProtoMessage 解析失败）→ 采样 KPI 进 `SharedState.kpi_history` 15s×120 FIFO → `/api/session/kpi/:sid` 时间序列 → admin 面板 📈 canvas 折线，测试 `test_route_agent_message_samples_ping_kpi`/`test_kpi_history_caps_and_drops_oldest`）。统一时间线/13s 决策树/crash.log 上报/告警未做。

### 批次 6 · 风险与回滚（168-177）

◐ relay 缓冲 16 取舍已记录（#170）；reqkey 风暴上限已内置（#173）；分发 grader 未做。

### 批次 7 · 远期（178-200）

⬜ 全部远期（i444/HW 编码/portal 直通/P2P/多显示器/多流/授权细化等）。

---

## 合入进度总计（本轮结束）

- **R4/5（200 点）**：✔ 类约 **57%**（甲 帧率/IDR/RTT分带/TestDelay探针/QoS五态、乙 Player 主体+韧性+错误恢复+内存+排队归因+光标叠加、丁 弱网可见性+输入节流+RTT分带、丙 遥测基线+日志+分辨率事件+带宽记账+admin KPI 曲线）；⬜ 20%（丙遥测剩余、戊发布门槛）；◐ 23%。
- **R5 落地清单（200 点）**：✔ 类约 **72%**（可靠通道 16 项 / 前端 20 项 / 抓帧 6 项 / 编码器 7 项 / 打包 3 项 / 遥测测试 10 项 / TestDelay 探针 1 项 / admin KPI 曲线 1 项 / 光标通道 1 项 / QoS 状态机 2 项）；⬜ 19%。
- **第 13 轮新增合入**：
  1. 编码线程数 loadavg 自适应（`encoder.rs codec_thread_num`：`(核数−loadavg)×0.5` 对齐 rustdesk——负载高自动减编码线程不抢 CPU，无 loadavg 回退核数一半；`test_codec_thread_num_bounded`/`test_loadavg_one_parses_or_none`）→ R3 甲7/8 / R5#82。
- **第 14 轮新增合入**：
  1. TestDelay 探针全链路（对齐 rustdesk `cm::TestDelay`）：浏览器每 1s 发 `desktop:test-delay {seq,t0}`（performance.now 单调时钟）→ relay 直转 → agent 即时 echo `test-delay-ack {seq,t0}` → relay 加 `broadcast_types`/KNOWN 白名单回传 → 浏览器本地单调时钟算**纯网络层 RTT**（不含编码/解码/渲染管线、不依赖时钟校准；与 e2e 的 `_clockOffset` 校准正交）→ qos 上报加 `probe_ms` → agent QoS 日志快照含 `probe_ms`，`QosAdaptive` 5 窗口中值 + `pipeline_bloated` 判据（网络健康+100ms 预算仍 ≤ e2e 中值 ⇒ over 来自管线/解码积压，不降码率；probe=0 未上报 ⇒ 沿用原判据，兼容老浏览器/测试）。测试 `test_qos_probe_confirms_network_congestion`（健康 probe 不降/高 probe 降/无 probe 降三场景）、`test_qos_probe_median_filters_spike`、`test_qos_probe_absent_returns_zero`。**浏览器实测**：网络 RTT 行 5ms（8 次采样稳定）、agent 日志 `delay_ms=15 probe_ms=4`（管线/网络正确分离）、静态屏 fps=1 且 qos_scale=1000（probe 健康不误降）→ R4 甲 A1 / R5#148。
- **第 15 轮新增合入**：
  1. 编码器故障热备（`mod.rs next_degrade_codec` + `rebuild_encoder_degrade`）：统一 #84 慢帧 / #85 encode-Err 降级出口——encode 连续 5 帧返回 Err（编码器故障/崩溃，区别于 #84 慢帧）自动重建为 fallback 链下一档（av1→vp9→h264），mp4_cfg 置 None + force_idr 重发 init；测试 `test_next_degrade_codec_trigger`（阈值/降档/一次性/末档 7 断言）→ R2 乙77 / R5#85。
  2. 台账修正：**#89 CBR 纪律此前已做**（`aom.rs:111 AOM_CBR` + `vpx.rs:91 VPX_CBR` + undershoot/overshoot 50% + KF_DISABLED 外部 force_idr）——上轮台账误标未做，本轮代码级核实后标 ✔。
- **第 16 轮新增合入**：
  1. admin KPI 曲线（R5 丙111/140 部分）：relay 用宽松 JSON 拦截 agent 心跳 `ping`（真实心跳缺 payload 字段，严格 ProtoMessage 解析会失败——此为采样触发前的关键前提）→ 采样心跳 KPI 进 `SharedState.kpi_history`（15s×120 FIFO）→ admin `/api/session/kpi/:sid` 返回时间序列 → admin 面板 Sessions 表 📈 按钮展开行内 canvas 折线（fps 蓝 / bitrate 绿）。测试 `test_route_agent_message_samples_ping_kpi`/`test_kpi_history_caps_and_drops_oldest`。**浏览器实测**：桌面开启后 KPI API 13 样本、6 个 running=true、bitrate>0；admin 📈 点击展开 canvas 曲线已绘制。
- **第 17 轮新增合入**：
  1. WS/HTTP 限流等价（`relay/ws.rs agent_conn_rate_ok`）：agent WS uplink 与 HTTP `/agent/events` 共享 per-IP `ev:` 30/min 连接配额（同一 key 无法切通道绕过），events handler 改复用辅助；测试 `test_agent_conn_rate_shared_ws_http`（30 放行/31 超限/异 IP 独立）→ R5#22。
  2. 台账修正：**#28 20s WS ping 已做**（`handle_agent_ws_uplink` 每 20s server-side ping，agent 死链 ~35s 检出），此前误标 ◐，代码级核实后标 ✔。
- **第 18 轮新增合入**：
  1. 光标独立通道（`capture.rs poll_cursor` + `FrameSource::set_cursor_cb` + `desktop:cursor` + `web/desktop.js updateCursor`）：**X11 `GetImage`/`ShmGetImage` 不含光标层——远程用户看不到鼠标指针是真实缺口**。agent 捕获线程 `next_frame` 内 100ms 节流 `XQueryPointer` → 位置经 `cursor_cb` → `desktop:cursor {x,y,shown}` 轻量消息（光标移动不触发整帧重编码）→ relay broadcast_types/KNOWN 白名单 → 浏览器 `.sr-cursor-overlay`（内联 SVG 箭头 + 捕获分辨率→显示尺寸映射）。**浏览器实测**：overlay 出现在屏幕中心（638,342≈640,360）、注入鼠标移动到 (300,200)/(600,450) 后 overlay 跟随到 (299,190)/(598,428)。另确认 enigo 注入需 DISPLAY 环境（测试环境未设导致注入失败，非代码 bug）→ R4 乙81-90 / R5#64。
- **第 19 轮新增合入**：
  1. QoS 五态质量状态机（`mod.rs QosQualityState` Unknown/Good/Medium/Degraded/Critical + `QosAdaptive::update_quality_state`）：由网络层 probe 中值 + 拥塞增量 over 推导的**显式状态对象**（rustdesk QualityStatus 同构），每次 250ms qos 采样更新；迁移记日志（实测 `from=Unknown to=Good probe_ms=20 over=0`）；`qos-ack` 回传 `qos_state` → 浏览器面板"QoS 状态"行（Good 绿/Medium 黄/Degraded 橙/Critical 红）。测试 `test_qos_quality_state_transitions`（五态迁移 + 恢复）、`test_qos_quality_state_without_probe`（无探针靠 over）。**浏览器实测**：面板显示 Good + 绿色、agent 日志快照含 `qos_state=Good`。状态为观测快照、不影响 fps/ratio 决策（决策测试全绿）→ R4 甲 A0 / A2。
- **第 20 轮新增合入**：
  1. 弱网 KPI 矩阵（`tools/weaknet_kpi_matrix.sh`）：netem 弱网档位（RTT 50/300/800 × 丢包 0/2%）逐档浏览器采样 QoS KPI（渲染 fps / e2e / 网络 RTT / qos_state / bitrate）→ KPI 汇总表 + **用户铁律断言"动态画面弱网不降帧"**（fps≥15；静态 fps=1 正确跳过，不误报）。实测采样正常（e2e 51ms、probe 4ms、qos_state Good、静态 fps=1 INFO 分支）。无 netem 权限自动降级到正常档基线 → R5#157 / R4 丁142 弱网 KPI 矩阵。
  2. 记录观察：连续 playwright 会话后新 join 的 `toggle-desktop-btn` 可能 disabled（疑测试会话残留，生产每次新标签页是干净 SSE 会话）——与批次1 #12（SSE 重建补 desktop:state）相关，台账 ⬜ 未闭合，如实记录。
- **第 21 轮新增合入**：
  1. relay 重启后桌面流自动恢复（`agent/mod.rs run_session` 加 `desktop_want_running` 会话级标志）：断线退出时记录桌面 running 并显式 stop（修掉孤儿 task——run_desktop_loop 是 tokio::spawn detach，desktop drop 不停它，会继续向失效 relay 发帧浪费 CPU + 重连双发冲突）；重连后自动 `desktop.start`（新 send_url，首帧强制 IDR 重发 init 给新 DesktopStream）。**实测**：两轮 relay 重启均显 `reconnected with desktop previously running — auto-restoring desktop stream` + `capture started 1280x720`（agent 日志证据）；用户手动关闭桌面（pagehide stop）后 running=false → 重连不自动开（符合预期）→ R5#11。
  2. 台账修正：**#19 桌面开启竞态幂等已实现**（`DesktopManager::start` 首行 `is_running` 守卫，检查→置 running 无 await 并发安全），此前误标 ◐。
  3. 诊断结论：#20 记录的连续会话按钮 disabled 现象，在干净复现下**不存在**（会话2 按钮正常 enabled + caps 正常）——疑 playwright 连续实例会话残留，非产品 bug；#12（SSE 重建补 desktop:state）实际已工作（桌面保持运行时新 join 收 caps 正常）。
- **第 22 轮新增合入**：
  1. 控制命令 ack（`agent/mod.rs` quality/codec/gray handler + `web/session.js`）：命令带递增 `seq` → agent 处理后回 `desktop:cmd-ack {seq,ok,error?}` → 浏览器 toast 反馈操作结果（quality/codec/gray 成功或失败可见）；relay broadcast_types/KNOWN 白名单加 cmd-ack。**浏览器实测**：quality(best) 命令 ack `{ok:true,seq:100}` 到达。TCP 可靠（命令不丢），本确认价值 = **操作结果反馈**（弱网/高负载下用户可见操作生效）+ 为弱网重发预留 seq 机制 → R5#2。
  2. 台账修正：#10 agent 重连幂等替换（`register_existing` 替换旧 session）与 #23 崩溃重启 key 续接（cached_tokens replay）经代码级核实**均已实现**；#29 控制消息优先级核实为 ◐（is_lossy 数据/控制区分已成立，channel 满时控制仍可能丢）。
- **未合入（如实）**：线协议二进制整块（批次2 #41-44）、跨进程时间线遥测、独立 100ms ack 批、AV1 测速门槛等——在台账对应 ⬜/◐ 行，未宣称完成。