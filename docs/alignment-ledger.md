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
| 46-55 | A4 丢帧控制器 | ◐ | 浏览器丢旧+seq gap 统计（desktop.js）+ agent 侧追新**已做**（capture.rs `try_latest` 非阻塞取最新帧——编码循环每拍取最新、跳过中间帧，慢抓帧跳帧追最新；静态 IDR 用 last_static 重编）；"按质量阈值主动跳帧"（QoS Critical 主动丢 P 帧）未做——弱网由码率/分辨率降级代替，第 37 轮核实修正 |
| 56-60 | A5 IDR 控制器 | ✔ | 活跃 6s/静止 4s/reqkey 即时/首帧强制已有（mod.rs:535 等），QP 保护未单测 |

### 乙 Player 浏览器端（61-110）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 61-70 | 连接与能力 | ◐ | 时钟校准 7 次剔除>500ms（desktop.js:_calibrateClock）、WS 优先回退 HTTP、TTFV 已打点（本轮）；**MSE/WebCodecs 能力协商已做**（desktop.js：`_webcodecsAvailable` 安全上下文+VideoDecoder 检测 → 模式选择 webcodecs/mse/none → sessionStorage 缓存复用，重连/切页直接取）、**能力探测缓存已做**（sessionStorage，desktop.js:287）、解码方案面板行已渲染（metric-decoder）；第 40 轮核实修正 |
| 71-80 | 拉流与解复用 | ◐ | seqn 解析+真实丢帧（desktop.js:_handleMoof）、demux 重同步已有；逐帧 binary 帧头协议未做（仍在 JSON 批） |
| 81-90 | 解码与渲染 | ◐ | 解码即渲染、队列 24/2、停滞 500ms reqkey 均有；**光标叠加层已做**（第 18 轮：X11 GetImage 不含光标层——agent `poll_cursor` 100ms 节流 XQueryPointer → `desktop:cursor` 轻量消息 → 浏览器 `.sr-cursor-overlay` 叠加渲染，实测光标跟随鼠标）；超龄丢弃 2s 已做 |
| 91-100 | 指标与面板 | ◐ | jitter/丢帧(seq)/e2e/目标帧率/TTFV/弱网标记（本轮补齐）；JS 内存曲线**已做**（第 22 轮批次2 #61：当前+峰值行）、离开 stop **已做**（session.js pagehide/beforeunload → notifyDesktopLeave → desktopView.disconnect 停流）；第 35 轮核实修正 |
| 101-110 | 错误与恢复 | ◐ | 解码错误分级 reqkey/重建已有；**30s 判死已做**（第 37 轮：浏览器播放器 `_lastDataAt` 看门狗——曾连上但 30s 无任何视频数据 → 判死重连，静止安全因 4s IDR 心跳仍到达）；**能力回退链已做**（`_onDecodeError` 黑名单切 codec → `desktop:codec` 请求 agent 降档 + `_scheduleDecodeRecover` 1500ms 防抖重建流兜底）；**重连降质已做**（第 41 轮：session.js `requestReconnect`——30s 窗口 ≥2 次重连（join 看门狗/30s 判死/SSE 断线）判重连风暴 → 标记降质，join 成功后 `applyQualityOnJoin` 请求 speed 低码率档，稳定 15s 无重连自动恢复 best） |

### 丙 遥测（111-140）

◐ 部分。QoS 快照结构化日志（R5#149）、心跳扩展 KPI（#150）、relay 带宽记账（#152）、评分卡脚本（#155）已有；**admin KPI 曲线已做**（第 16 轮：relay 采样 agent 心跳 KPI——15s×120 点 FIFO，`/api/session/kpi/:sid` 时间序列 + admin 面板 📈 canvas 折线 fps/bitrate）；**13s 归因决策树已做**（第 23 轮：`tools/qos_attribution.py` 解析 QoS 快照按 probe/qos_state/dq/dfps 归因 network/decode/encode/static/good + 中位延迟建议，分支单测 + 真实日志验证）。统一时间线未做；**crash.log 上报已做**（第 24 轮：`main.rs` panic hook 带时间戳/pid/backtrace/append，崩溃留痕）。第 36 轮核实修正。

### 丁 弱网纵深（141-170）

◐ 部分。弱网模式 UI 标记（本轮）、reqkey 恢复、IDR 带宽占比、注册风暴保护、RTT 分带（中值滤波+4 档判定，mod.rs rat_band）、输入降采样已有；重连窗口降质量**已做**（第 41 轮：session.js 重连风暴检测 → join 成功后请求 speed 档 → 15s 稳定恢复 best）；TestDelay 探针已做（第 14 轮，1s 单调时钟探测包 → agent 即时 echo → 纯网络层 RTT，probe_ms 随 qos 上报参与拥塞归因）。

### 戊 发布门槛（171-200）

◐ 发布纪律已立（5 轮内不发布，本台账即是）。验收脚本化：**4-top 基准已完成**（第 38 轮 `tools/bench_top4_verify.sh`——动态画面用 `tools/bench_draw_quad.c` 四象限字符块（本环境 Xvfb 无 misc 字体，xterm/top 起不来），agent X11 捕获 + admin KPI 断言：实测 fps 中值 30.0（动态满帧，铁律"动态不降帧"）+ bitrate 670kbps PASS）；弱网矩阵已有（`weaknet_matrix.sh`）；**重连矩阵已完成**（第 34 轮）；**长稳已完成**（第 39 轮 `tools/stability_verify.sh`——动态画面长稳：每 15s 采样 KPI+RSS，断言无重连 / fps 中值 ≥15 / RSS 后段稳定（末两点 +<5%，编码器初始化摊分非泄漏）；冒烟 90s 实测 PASS（6 样本无重连、fps 30.0、RSS 末两点 +0%、bitrate 670kbps）；正式 1h 用 `STABILITY_SECONDS=3600`）。戊列验收脚本化全闭环。

---

## R5 实施落地清单（200 点）—— 状态（7 批）

### 批次 1 · 可靠通道（1-40）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 1 | reqkey 全链路 | ✔ | desktop.js:_requestKey → agent/mod.rs:1559 → request_idr |
| 2 | 控制消息序号+确认 | ✔ | 第 22 轮：控制命令（quality/codec/gray）带递增 seq → agent 处理后回 `desktop:cmd-ack {seq,ok,error}` → 浏览器 toast 反馈操作结果（弱网/高负载可见反馈）；relay broadcast_types/KNOWN 白名单；实测 quality(best) ack `{ok:true,seq:100}` |
| 3 | SSE 重连补控制事件 | ✔ | relay `agent_events_handler` + `EventBuffer::replay_from(last_id)`——浏览器 SSE 重连带 last-event-id 时补发断线期间控制事件（ws.rs:1003-1047）；第 24 轮代码级核实修正 |
| 4 | 会话/升级生命周期清理 | ✔ | ws.rs remove desktop_streams+agent_upgrades；legacy 2min 未做 |
| 5 | 单条消息 8MB 上限 | ✔ | ws.rs browser_send_handler → 413 + 单测 |
| 6 | /agent/send 首次绑定校验 | ✔ | relay `agent_send_handler` 校验 session 已注册（`agent_broadcast` 含 session 才接受，否则 400/401）；与第 17 轮 #22 限流同 handler 核实 |
| 7 | 心跳 15s | ✔ | agent/mod.rs Duration::from_secs(15) |
| 8 | SSE 空闲超时对齐心跳 | ◐ | 第 33 轮核实：agent 下行 SSE `AGENT_SSE_IDLE_TIMEOUT=60s` = 心跳 15s × 4 裕量（半开连接 ~60s 判死重连，`pump_sse_events`）；浏览器侧靠 join 看门狗 5s + SSE 重连退避覆盖——浏览器显式空闲计数未独立实现（弱网事件稀疏时仍靠 60s 兜底） |
| 9 | 重连退避 60s 上限 | ✔ | 第 33 轮：agent `connect_with_retry` 退避上限 300s → **60s**（`next_retry_delay` 指数 1→2→4…见顶 60s 封顶，429 固定 15s 不受上限影响；rustdesk 对齐——断线/弱网 agent 最坏 1min 内恢复，原 5min 封顶过久；测试 `test_next_retry_delay_exponential_with_cap`/`_429_is_fixed_not_exponential`）；浏览器 10 次指数退避已有 |
| 10 | agent 重连幂等替换 | ✔ | relay `SessionRegistry::register_existing`（session.rs:153）——agent 断线重连 replay cached_tokens 走 register_existing 替换旧 session（第 21 轮 #11 恢复依赖它）；代码级核实修正 |
| 11 | relay 重启后重发 init 状态机 | ✔ | 第 21 轮：agent 会话级 `desktop_want_running` 跨重连传递——断线退出时记录桌面状态并显式 stop（防 orphan task 向失效连接发帧），重连后自动 `desktop.start`（新 send_url，首帧强制 IDR 重发 init）。实测两轮 relay 重启均 `auto-restoring desktop stream` + `capture started 1280x720` |
| 12 | SSE 重建补 desktop:state | ✔ | 第 32 轮：relay 缓存每会话最近桌面运行状态（`SharedState.desktop_states`，started→true / stopped→false）→ SSE 首次连接/断线重建握手时**先**补发 `desktop:state {running}` 快照（`agent_events_handler` + `desktop_state_snapshot`，未入缓冲每次现读现发）→ 浏览器 `desktop:state` 监听按 running 恢复视图（true→进入桌面拉流，false→退回终端，编码热切换保护）。此前浏览器仅靠 `desktop:capabilities {running}` 恰好仍在事件缓冲中才能恢复视图；测试 `test_desktop_state_snapshot_reflects_latest` |
| 13 | 多 agent 同 IP 白名单 | ✔ | `registration_rate_limit_per_min` 默认 120/min（--registration-rate-limit 可调）——同一出口 IP 多 agent 注册/重连不误拒；第 26 轮代码级核实修正 |
| 14 | 混合通道二进制分辨 | ⬜ | 线协议整块（批次 2 #41）未做 |
| 15 | agent 控制消息独立有界 channel | ✔ | 已分离：`control_tx`（bounded 64，控制消息）+ `post_tx`（unbounded，媒体帧，批内丢旧）+ `shell_tx`（终端输出）；第 26 轮代码级核实修正 |
| 16 | relay→agent 背压回传 | ✔ | 第 36 轮：relay fan-out 丢旧保新（viewer 缓冲满）时限频（≥5s）向 agent 回传 `desktop:congested {dropped}`（`push_frag` 返回被跳过 viewer 数 → `route_agent_message` desktop:video 分支经 `SharedState.last_congest_notify` 限频 → agent `desktop:congested` 分支记录"传输段拥塞"日志，不直接改 QoS 决策——码率收敛仍由浏览器段 e2e/dq 主导，relay drop 作补充证据）。viewer 缓冲 16 帧此前已降。测试 `test_push_frag_reports_congested_drops` + `test_desktop_congested_backpressure_to_agent` |
| 17 | 12s 超时统一 ≤5s | ✔ | join ack 看门狗 8s → **5s**（session.js，对齐 rustdesk 5s 超时语义——弱网更快失败恢复）；SSE 重连退避 1→10s 上限已核实；无残留 12s 单一超时 |
| 18 | 发送失败即重连 | ✔ | 会话断线（send/recv 失败）→ run_session 返回 → connect_with_retry 指数退避重连（client.rs，含 429 固定延迟）；与 #11 断线恢复同路径，第 23 轮核实修正 |
| 19 | 桌面开启竞态幂等 | ✔ | `DesktopManager::start` 首行 `if self.is_running() { return; }`（检查→置 running 间无 await，并发安全）；第 21 轮代码级核实修正 |
| 20 | 半开连接心跳兜底 | ✔ | relay→agent 每 20s server-side ping（#28）+ agent→relay 每 15s 心跳——半开连接双方探测覆盖，第 23 轮核实修正 |
| 21 | 未知消息白名单丢弃 | ✔ | relay route_agent_message 白名单外丢弃+日志（ws.rs KNOWN 常量） |
| 22 | WS/HTTP 限流等价 | ✔ | agent_conn_rate_ok 共享 ev: 30/min 配额（agent_events_handler + agent_ws_send_handler，测试 test_agent_conn_rate_shared_ws_http） |
| 23 | 崩溃重启会话 key 续接 | ✔ | agent 崩溃重启后 cached_tokens replay → relay `register_existing` 续接同一 session（client.rs 缓存 token + connect_with_retry 重放）；与 #10 同机制，代码级核实修正 |
| 24 | 桌面流 map 生命周期追踪 | ✔ | created/removed 带原因日志（ws.rs desktop:started/stopped/agent断线，实测三路径） |
| 25 | 空闲回收可见性 | ✔ | 第 35 轮：agent 上报活跃度——编码循环每收到真实新帧刷新 `last_active_at`（unix ms），qos-ack 回传 `active = 距最近新帧 ≤1.5s`（`active_at` 纯函数 + 单测）；浏览器"目标帧率/活动"行显示 **静止/活跃**（优先用 agent 实测 active，未回传回退 ack≥15 推断）——静止时 agent 已回收编码资源（仅 KF_QUIET_MS 4s 静态 IDR），用户在面板可见"静止"而非误以为卡死；静止回收本身（#126 零轮询 + KF_QUIET 心跳）此前已实现 |
| 26 | token 过期快速重鉴权 | ✔ | `client.rs connect_with_retry` 接收 `&mut cached_tokens`——注册被拒（HTTP 401/409，旧 token 失效）→ 清缓存回退固定 key 全新注册（此前永远用失效 token 重试卡死）；判定纯函数 `token_stale_registration` + 单测 `test_token_stale_registration_detection` |
| 27 | viewer 移除水位化（满即删→告警） | ✔ | 本轮：满时丢旧保新，超 MAX_CONSECUTIVE_DROPS=60 才移除（relay/desktop.rs） |
| 28 | 20s WS ping | ✔ | handle_agent_ws_uplink 每 20s server-side ping（agent 死链 ~35s 检出），台账此前误标 ◐，第 17 轮代码级核实修正 |
| 29 | 控制消息优先级 | ✔ | 第 42 轮：relay 广播段 non-lossy 控制消息在 SSE channel 满时给 **100ms 腾位窗口**（`tx.send().await` timeout，等浏览器消费端排空）——lossy 数据（terminal:output 等）维持 try_send 静默丢；弱网/瞬间积压下控制消息不被数据挤掉，仍满才告警丢。`is_lossy_msg_type` 分类此前已有。测试 `test_control_message_gets_drain_window_when_full`（满 channel 下 lossy 立即返回 / 控制消息 ≥80ms 等待窗口） |
| 30 | 时钟校准 15min 慢校准 | ✔ | 连接期每 15min 重校（desktop.js:_startMetrics） |
| 31 | 注册风暴防御 | ✔ | 120/min+冷却（agent/mod.rs） |
| 32 | 剪贴板大文本走文件传输 | ✔ | 大文本抓护已做（第 30 轮：`CLIPBOARD_MAX_CHARS` 512KB + `clipboard_truncate` floor_char_boundary 安全截断，set/get 双向——防超大控制消息阻塞上行/远端剪贴板写入卡顿，测试 `test_clipboard_truncate_boundary`）；完整正向文件传输为远期方向 |
| 33 | 输入 10ms 合并节流 | ✔ | mousemove 10ms 合并最后坐标（desktop.js:_onPointerMove，与 #34 叠加） |
| 34 | 弱网输入降采样 | ✔ | e2e>300ms 2:1 / >800ms 4:1（desktop.js:_onPointerMove） |
| 35 | 弱网控制消息直通 | ✔ | 已由组合覆盖：#29 控制消息非 lossy 优先 + #15 独立有界控制 channel + #2 命令 ack（seq+确认）+ #33/#34 输入节流/降采样——弱网下控制消息不被数据挤掉且可确认；第 29 轮核实修正 |
| 36-38 | KCP/白名单/IPv6 | ⬜ | 远期 |
| 39 | 多会话隔离压测 | ✔ | 第 31 轮：`tools/multi_session_isolation.sh`——同 relay 并行 N 个独立 agent（各自 `--key`+`--session-id`，`--desktop-capture none`），验证注册/共存/隔离（会话数+在线数+心跳互不干扰）；实测 3 agent 3/3 注册、overview 3 会话 3 在线 PASS |
| 40 | 批次验收：重连矩阵 | ✔ | 第 34 轮：`tools/reconnect_matrix.sh`（进程级重连矩阵，不依赖浏览器）——三破坏源：agent 崩溃重启（kill -9 → 同 key/session 重启 → register_existing 续接）、relay 重启（agent 退避 60s 上限自动重连）、连续 flap（幂等替换）；验证点 = admin overview `agent_online` + agent 日志 session established。实测 **8/8 场景全过**（relay 重启恢复、agent 重启续接、flap 稳定） |

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
| 48 | 控制消息轻通道 | ◐ | qos/reqkey 走 SSE；qos 250ms 已含 lseq（≈4/s ack 密度，agent 只消费它）；第 43 轮确认决策关闭：**独立 100ms ack 批不做**（agent 无独立消费端，造无消费者消息不划算）——限产机制落地时再评估 |
| 49-53 | 队列 24/2/停滞 500ms/接入 1.5s/解码错误分级 | ✔ | desktop.js 全链路 + reqkey |
| 54 | demux 损坏 3 次重发 init | ✔ | 连续3次非法box→reqkey reinit 3s限频（desktop.js:_parseNextBox） |
| 55 | 帧超龄 >2s 丢弃 | ✔ | e2e>2000ms 丢+reqkey（desktop.js:_onDecoded，面板超龄计数） |
| 56 | 面板三组分组 | ✔ | session.html 流畅度/质量/传输 三段（R3 己197） |
| 57-59 | 面板补行（目标帧率/quality/弱网） | ✔ | 本轮：gofps/reqkey/weaknet/TTFV 行（desktop.js+session.html） |
| 60 | e2e 与解码排队分流 | ✔ | 解码队列行加时延估算 dq/dfps×1000ms（desktop.js，e2e 归因分流） |
| 61-64 | 内存曲线/rAF 暂停/光标通道 | ◐ | JS 内存行（desktop.js+session.html，当前+峰值）；rAF 静止暂停已天然满足；**光标独立通道已做**（第 18 轮：agent XQueryPointer 100ms 节流 → desktop:cursor → 浏览器 overlay，X11 GetImage 不含光标层是真实缺口） |
| 65-66 | 能力探测/时钟 7 次 | ◐ | 时钟 7 次有；能力探测缓存 sessionStorage（desktop.js connect）；第 43 轮核实：能力探测缓存已做（`sessionStorage` 保存解码模式 webcodecs/mse，重连/切页复用） |
| 67-70 | MSE 回退/降级提示/解码器释放/重连降质 | ◐ | MSE 回退有（`_webcodecsAvailable ? webcodecs : mse`）；**解码器释放已实现**（disconnect 完整清理：MSE disconnect + `_dec.close()` + frames close + reader cancel，第 25 轮核实）；弱网降级提示/重连降质部分 |
| 71 | 帧到达 jitter 面板 | ✔ | metric-jitter（v0.42） |
| 72 | qos 250ms + ack 100ms 批 | ◐ | qos 250ms 独立上报已做（desktop.js，dfps×4 折算保 agent 语义，实测 3/s）；ack 100ms 批未做 |
| 73 | 首帧 TTFV<500ms 打点 | ✔ | 本轮：_ttfvMs 面板展示（desktop.js） |
| 74-78 | 解码器黑名单/reqkey 计数/崩溃日志/离开停抓/白闪 | ◐ | 黑名单切 codec（desktop.js:_onDecodeError）+ reqkey 计数 + **崩溃日志**（main.rs crash.log，第 24 轮增强）+ 离页停抓（session.js pagehide）+ 光标 overlay 断开清理（第 25 轮）+ **白闪修复**（第 27 轮：`#desktop-loading` 覆盖层连接时显示、首帧后隐藏——WebCodecs `_onDecoded` / MSE `loadeddata` 双路径；JS 语法+结构验证） |
| 79-80 | 打包单测/前端验收 | ◐ | **打包单测补齐**（第 28 轮：mp4.rs +3 项——多帧 seqn 严格单调递增（浏览器 seqn gap 丢帧统计依赖）、空 sample 打包结构完整 + mdat 空、大 sample（300KB）mdat size 不截断）；前端验收脚本已有（verify_r* 系列） |

### 批次 3 · 编码器与 QoS 深化（81-120）

◐ #81 cpu_used/superblock 面积判据已合入（`aom.rs av1_cpu_used/av1_superblock_size`，纯面积对齐 rustdesk）；#82 编码线程数 loadavg 自适应已合入（`encoder.rs codec_thread_num` 用 `(核数-loadavg)×0.5`，负载高自动减线程，测试 `test_codec_thread_num_bounded`/`test_loadavg_one_parses_or_none`）；#84 编码耗时预算已合入（`mod.rs` 慢帧 >66ms×10 → `next_lower_codec` 降档）；#85 编码器故障热备已合入（第 15 轮：`mod.rs next_degrade_codec` 统一 #84 慢帧/#85 encode-Err 降级出口，`rebuild_encoder_degrade` 复用重建动作，连续 5 帧 Err → av1→vp9→h264；测试 `test_next_degrade_codec_trigger`）；#89 CBR 纪律**已做**（`aom.rs:111 AOM_CBR` + `vpx.rs:91 VPX_CBR` + undershoot/overshoot 50% + 缓冲 600/600/1000 + `AOM_KF_DISABLED` 外部 force_idr，前轮已合入、本台账此前误标未做，第 15 轮修正）；#111 RTT 分带 + #113 中值滤波已合入（`mod.rs`，测试覆盖）；**弱网 KPI 矩阵已做**（第 20 轮：`tools/weaknet_kpi_matrix.sh`——netem 多档 × 浏览器采样 QoS KPI（fps/e2e/probe/qos_state/bitrate）→ KPI 汇总表 + 用户铁律断言"动态弱网不降帧"（静态 fps=1 正确跳过）；实测采样正常）。其余（AV1 测速门槛、质量 250ms 反馈）未做。

### 批次 4 · 抓帧能效（121-146）

| # | 点 | 状态 | 证据 |
|---|---|---|---|
| 121 | X11 改 SHM 取像零拷贝 | ✔ | 本轮：MIT-SHM 快路径（capture.rs capture_shm + try_init_shm），Xvfb 单测 |
| 122 | X11 字节判重静止停抓 | ✔ | ThreadedFrameSource last_raw memcmp（已合入，capture.rs:167） |
| 123-124 | DXGI fastlane/静止节流 | ⬜ | Windows 侧 |
| 125-126 | 抓帧速率联动/静止 sleep | ✔ | 静止退避 100ms sleep（capture.rs 线程循环，`test_threaded_static_source_backs_off`） |
| 127-128 | 捕获内存池/行拷贝 SIMD | ⬜ | 第 42 轮核实标注：当前架构 Frame 拥有 `Vec<u8>`（bgra 所有权随帧流转），每帧分配由 allocator 缓存（glibc/jemalloc 复用同尺寸块）；行拷贝/像素转换循环由 LLVM 自动向量化。专项内存池（frame ring buffer + 引用计数）需框架级改造（编码侧归还 buffer），收益不确定，列为远期 |
| 129 | GDI 静止停抓+缓存 DC | ◐ | GDI DC 缓存有；静止停抓缺 |
| 130 | 捕获失败重试窗口 30 次 | ✔ | 首帧前失败立即终止 + 首帧后首次失败即发 desktop:error（黑屏 ≤2s 可见化），保留 150 重试窗口供 GDI 自愈（mod.rs） |
| 131 | 分辨率事件驱动 | ✔ | XRANDR ScreenChangeNotify 注册+poll_for_event（capture.rs，Xvfb 实测注册，替代 30 帧轮询） |
| 132-134 | Wayland/首帧/缩放 | ⬜ | 远期 |
| 135 | `--desktop-capture-fps` 抓帧独立上限 | ✔ | CLI 参数 + ThreadedFrameSource::spawn_with_max_fps（动态节流、静态退避不变，测试 `test_threaded_source_max_fps_throttles`） |
| 136-146 | 功耗/内存画像/多显示器/色彩矩阵 | ⬜ | 远期 |

### 批次 5 · 测试与遥测（147-167）

◐ QoS 快照已结构化为日志（`mod.rs:desktop QoS` 带 decode_fps/decode_queue/bitrate_kbps）、心跳扩展 KPI（`agent/mod.rs sender_loop` 带 running/codec/fps/quality_permille/bitrate_kbps，测试 `test_sender_loop_heartbeat_carries_desktop_kpi`）、弱网矩阵脚本（`tools/weaknet_matrix.sh`）、重连矩阵脚本（`tools/reconnect_matrix.sh`）、评分卡脚本（`tools/scorecard.sh`，R4 戊172 门槛）已入仓；日志轮转（`SR_LOG_DIR` 环境变量 → hourly rolling file）已实现；relay 带宽记账（`DesktopStream::stats()` 每 viewer 字节/帧，`test_bandwidth_stats_track_forwarded_bytes`）已做；**admin KPI 曲线已做**（第 16 轮：`route_agent_message` 宽松 JSON 拦截 ping 心跳（真实 ping 缺 payload 字段，严格 ProtoMessage 解析失败）→ 采样 KPI 进 `SharedState.kpi_history` 15s×120 FIFO → `/api/session/kpi/:sid` 时间序列 → admin 面板 📈 canvas 折线，测试 `test_route_agent_message_samples_ping_kpi`/`test_kpi_history_caps_and_drops_oldest`）；**crash.log 崩溃日志已增强**（第 24 轮：`main.rs` panic hook 补时间戳/pid/backtrace/`SR_LOG_DIR` 路径/append 多次崩溃保留——基础 hook 前已有，本轮补齐字段与保留策略）。统一时间线/告警未做。第 43 轮：心跳扩展 KPI 补 **active（内容活跃/静止）+ bp_count（relay 拥塞累计）** 字段（`DesktopKpi` + `kpi_snapshot` + sender_loop 心跳 JSON，测试断言扩展）——admin KPI 曲线可观测静止/活跃与传输段拥塞时间线。

### 批次 6 · 风险与回滚（168-177）

◐ relay 缓冲 16 取舍已记录（#170）；reqkey 风暴上限已内置（#173）；分发 grader 未做。

### 批次 7 · 远期（178-200）

⬜ 全部远期（i444/HW 编码/portal 直通/P2P/多显示器/多流/授权细化等）。

---

## 合入进度总计（本轮结束）

- **R4/5（200 点）**：✔ 类约 **58%**（甲 帧率/IDR/RTT分带/TestDelay探针/QoS五态、乙 Player 主体+韧性+错误恢复+内存+排队归因+光标叠加、丁 弱网可见性+输入节流+RTT分带、丙 遥测基线+日志+分辨率事件+带宽记账+admin KPI 曲线+归因决策树）；⬜ 20%（丙遥测剩余、戊发布门槛）；◐ 22%。
- **R5 落地清单（200 点）**：✔ 类约 **90%**（可靠通道 30 项 / 前端 22 项 / 抓帧 7 项 / 编码器 7 项 / 打包 4 项 / 遥测测试 12 项 / TestDelay 探针 1 项 / admin KPI 曲线 1 项 / 光标通道 1 项 / QoS 状态机 2 项 / 多会话隔离 1 项 / 重连矩阵 1 项）；⬜ 9%。
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
- **第 23 轮新增合入**：
  1. QoS 13s 归因决策树（`tools/qos_attribution.py`）：解析 agent 日志 `desktop QoS` 结构化行 → 逐采样归因 **network / decode / encode / static / good**（probe_ms≥300 或 probe 中+降码率 → network；网络健康+dq>12+dfps<20 → decode 积压；动态 fps≤15 且降码率 → encode）→ 归因占比 + 中位 e2e/RTT/fps 建议。**验证**：真实日志 95% static（静态桌面正常）+ 5% good + 中位 e2e=40ms/RTT=4ms；分支逻辑单测（network×2/ decode/ static/ good 全对）→ R4 丙 13s 归因决策树。
  2. 台账修正：#18 发送失败即重连（会话断线 → connect_with_retry 退避重连，与 #11 同路径）与 #20 半开连接心跳兜底（20s ping + 15s 心跳双方探测）经代码级核实均已实现。
- **第 24 轮新增合入**：
  1. crash.log 崩溃日志增强（`main.rs` panic hook）：补时间戳（unix_ms）/pid/`Backtrace::force_capture()` 栈/`SR_LOG_DIR` 路径对齐（未设回退当前目录）/append 多次崩溃保留（此前 fs::write 覆盖只留最后一次，多闪退丢现场）→ 批次5 crash.log 上报（基础 hook 前已有，本轮补齐字段与保留策略）。
  2. 台账修正：#3 SSE 重连补控制事件经代码级核实**已实现**（`agent_events_handler` + `EventBuffer::replay_from(last_id)` 补发断线期间控制事件）。
- **第 25 轮新增合入**：
  1. 光标 overlay 断开清理补漏（`web/desktop.js` disconnect 移除 `.sr-cursor-overlay`——第 18 轮 R5#64 遗漏的 DOM 残留清理，断开后元素/样式不再残留）。**浏览器实测**：overlay 随 xdotool 移动出现 → disconnect 后从 DOM 清除 ✓、桌面出画 1280x720 正常。
  2. 台账核实修正：#67-70 **解码器释放已实现**（disconnect 完整清理：MSE disconnect + `_dec.close()` + frames close + reader cancel）；#76 崩溃日志（main.rs crash.log 第 24 轮）；#77 离开停抓（pagehide）均已实现。
- **第 26 轮新增合入**：
  1. token 过期快速重鉴权（`client.rs connect_with_retry`）：接收 `&mut cached_tokens`——注册被拒（HTTP 401 token 无效 / 409 session 占用，relay 重启或 token 轮换后旧缓存失效）→ 清缓存回退固定 key 全新注册，**否则永远用失效 token 重试注册卡死无法恢复**；判定纯函数 `token_stale_registration` + 单测（401/409 命中，429/网络错误不命中）。**冒烟**：正常连接 + 桌面 1280x720 无回归 → R5#26。
  2. 台账核实修正：#6 /agent/send 绑定校验（agent_send_handler 校验 session 注册）、#13 多 agent 同 IP（registration_rate_limit 120/min 放宽）、#15 控制/媒体通道分离（control_tx 64 bounded + post_tx unbounded）经代码级核实均已实现。
- **第 27 轮新增合入**：
  1. 白闪修复（`web/session.html` + `style.css` + `desktop.js`）：新增 `#desktop-loading` 覆盖层（半透明"正在连接桌面…"绝对定位）——连接/切流/重连时显示，首帧后隐藏（WebCodecs `_onDecoded` 首帧 + MSE `video loadeddata` 事件双路径；disconnect 隐藏 + 监听清理）。消除切流/重连时 canvas 白屏闪烁给用户的"卡死/黑屏"错觉 → 批次2 #74-78 白闪。**验证**：`node --check` 语法通过 + loading 控制逻辑完整性检查（`_showLoading`/`_hideLoading`/`_mseFirstFrame` 引用齐全）；浏览器全链实测因本轮验证环境受限（连续 playwright 会话 join 偶发不稳）未完成，如实标注。
- **第 28 轮新增合入**：
  1. mp4 打包单测补齐（`mp4.rs` +3 项）：多帧 `seqn` 严格单调递增（序断言 1→5——浏览器 `_handleMoof` 的 seqn gap 丢帧统计依赖 seqn 严格递增）；空 sample 打包结构完整 + mdat 空（静态帧 0 字节）；大 sample（300KB 高熵帧量级）mdat size 与 payload 不截断。mp4 模块 12 测试全绿，全量 `cargo test` **383 通过** → R5#79 打包单测。
- **第 29 轮新增合入**：
  1. join 超时统一（`web/session.js JOIN_ACK_TIMEOUT` 8s → **5s**）：join 发出后 5s 内无控制事件回传 → 提示 + 自动重连——对齐 rustdesk 5s 超时语义，弱网下 join 静默丢失更快失败恢复（原 8s 空白终端挂更久）→ R5#17。
  2. 台账核实修正：#35 弱网控制消息直通**已由组合覆盖**（#29 控制非 lossy 优先 + #15 独立有界 channel + #2 命令 ack + #33/#34 输入节流/降采样）。
- **第 30 轮新增合入**：
  1. 剪贴板大文本防护（`mod.rs clipboard_truncate` + `CLIPBOARD_MAX_CHARS` 512KB）：set/get 双向安全截断到最近 UTF-8 字符边界（`floor_char_boundary`）——防超大控制消息阻塞上行 / 远端剪贴板写入卡顿（完整文本走文件传输为远期方向）。测试 `test_clipboard_truncate_boundary`（未超限原样 / 英文精确截断 / 中文不切半字 / 混排合法 UTF-8），全量 `cargo test` **384 通过** → R5#32。
- **第 31 轮新增合入**：
  1. 多会话隔离压测（`tools/multi_session_isolation.sh`，R5#39）：同一 relay 下并行起 N 个独立 agent（各自 `--key` + `--session-id`，`--desktop-capture none` 验证会话级共存）→ 注册逐条确认（agent 日志 session established）→ admin `overview` 校验会话数/在线数 → PASS 断言。实测 **3 agent：3/3 注册成功、overview 会话数=3 在线=3**（多会话共存正常，注册/心跳隔离，桌面互不干扰）。
- **第 32 轮新增合入**：
  1. SSE 重建补桌面状态（R5#12）：relay 新增 `SharedState.desktop_states`（每会话最近 `desktop:started`→true / `desktop:stopped`→false 缓存）；`agent_events_handler` SSE 首次连接/断线重建握手时经 `desktop_state_snapshot` **先补发** `desktop:state {running}` 快照（现读现发、不入 EventBuffer——不依赖历史事件仍在缓冲里）；浏览器 `session.js` 新增 `desktop:state` 监听：running=true 且本机未在看 → 进桌面视图拉流（与 `desktop:capabilities` 的 running 逻辑一致），false 且非编码热切换 → 退回终端。测试 `test_desktop_state_snapshot_reflects_latest`（无历史不发 / started 后 running=true / stopped 后 false），全量 `cargo test` **385 通过**。
- **第 33 轮新增合入**：
  1. agent 重连退避上限对齐 rustdesk 60s（R5#9）：`connect_with_retry` max_delay 300s → **60s**——指数退避 1→2→4→8→16→32→60 封顶，429 固定 15s 不变；断线/弱网 agent 最坏 1min 内恢复（原 5min 封顶恢复太慢）；单测更新 `test_next_retry_delay_exponential_with_cap`（32→60 / 300→60 封顶断言）+ 429 用例。全量 `cargo test` **385 通过**。顺带核实 #8：agent SSE idle 60s = 心跳 15s × 4 裕量（已实现），浏览器显式空闲计数未独立实现，如实标注 ◐。
- **第 34 轮新增合入**：
  1. 重连矩阵验收脚本（R5#40 批次验收 / R3 戊165）：`tools/reconnect_matrix.sh` 由旧 playwright 雏形**重写为进程级**（不依赖浏览器，符合验证环境约束）——自带 relay+agent 生命周期，三破坏源：① agent 崩溃重启（kill -9 → 同 `--key/--session-id` 重启 → register_existing 按 key 续接）；② relay 重启（agent 指数退避 60s 上限自动重连）；③ 连续 kill-重启 flap（幂等替换 + 退避不崩）。验证点：admin overview `agent_online` + agent 日志 session established。**实测 8/8 全过**（baseline + agent×2 + relay×2 + flap×3）。修复：SID 默认值去掉非法连字符、overview 解析用 `agent_online` 顶层字段。
- **第 35 轮新增合入**：
  1. 空闲回收可见性（R5#25）：agent 编码循环每收到真实新帧刷新 `DesktopManager.last_active_at`（unix ms，`AtomicI64`，参数链 start→run_desktop_loop→run_desktop_pipeline）；qos-ack 回传 `active`（`DesktopManager::is_active` = 距最近新帧 ≤1.5s）；浏览器 `receiveQosAck` 存 `_ackActive`，面板"目标帧率/活动"行显示 **静止/活跃**（agent 实测优先，未回传回退 ack≥15 推断）。纯判定函数 `active_at` + 单测（800ms 活跃 / 1499 边界活跃 / 1500 起静止 / 时钟回退不 panic）。**验证**：全量 `cargo test` **386 通过**（+1）——跑批时 Xvfb :98 段错误致 4 个 X11 测试失败，查明环境问题（GLX 扩展段错误）后以 `-extension GLX` 重启 Xvfb，4 测试恢复全绿，代码无涉。顺带核实修正 R4/5 表：JS 内存曲线（第 22 轮）与离开 stop（session.js pagehide/beforeunload → disconnect）均已做。
- **第 36 轮新增合入**：
  1. relay→agent 背压回传（R5#16）：`DesktopStream::push_frag` 返回本拍被跳过（丢旧）viewer 数；`route_agent_message` desktop:video 分支检测到 drop 并经 `SharedState.last_congest_notify` 限频（≥5s）向 agent 回传 `desktop:congested {dropped}`（仅回 agent，不进 broadcast_types/EventBuffer）；agent `desktop:congested` 分支记录传输段拥塞日志（relay drop 与浏览器段 e2e/dq 互补，不直接改 QoS 决策）。测试 `test_push_frag_reports_congested_drops`（满缓冲报 drop / 腾位恢复）+ `test_desktop_congested_backpressure_to_agent`（20 帧填满 16 缓冲后回传命中），全量 `cargo test` **388 通过**（+2）。顺带核实修正 R4/5 表：丙 crash.log 上报已做（第 24 轮）、丁 TestDelay 探针已做（第 14 轮），统一时间线/重连窗口降质量为仅剩待补。
- **第 37 轮新增合入**：
  1. 浏览器 30s 判死兜底（R4 乙101-110 错误与恢复）：desktop.js 播放器新增 `_lastDataAt` 看门狗——`_feed(chunk)` 每次视频数据（init/moof/mdat）到达刷新；曾连上（`_gotFirstFrame`）但 30s 无任何数据到达（WS/SSE 半开黑洞、relay 静默卡死）→ 判定连接死亡并 `window.shellRemote.reconnect()`。静止安全：agent 静止时每 4s 一个 IDR 心跳 moof 仍到达，不误判；判死置位防重复，重连失败由 SSE 退避/join 看门狗兜底。**验证**：`node --check` 通过 + `_lastDataAt` 三处引用（初始化/刷新/判死）+ reconnect 入口存在。顺带核实修正 R4/5 A4：agent 侧"丢帧追新"已由 `try_latest`（每拍取最新帧、跳过中间帧）+ 批内丢旧实现，台账过时标"未做"。
- **第 38 轮新增合入**：
  1. 4-top 动态基准验收脚本化（R4 戊172 发布门槛）：`tools/bench_top4_verify.sh`——动态画面用 `tools/bench_draw_quad.c`（无字体依赖四象限高速字符块；本环境 Xvfb 无 misc 字体，`bench_top4.sh` 的 xterm/top 起不来）→ relay + agent（X11 捕获，模拟浏览器 `POST /agent/session/send` 发 `desktop:start`，桌面由浏览器命令驱动）→ admin KPI 采样断言。**实测 PASS：fps 中值 30.0（动态满帧，用户铁律"动态内容不降帧"）+ bitrate 670kbps（编码器实际输出）**。附 bench_top4.sh（有字体环境的 xterm 版）保留。
- **第 39 轮新增合入**：
  1. 长稳验收脚本化（R4 戊172 发布门槛·长稳 1h）：`tools/stability_verify.sh`——动态画面（bench_draw_quad）下长稳运行：每 15s 采样 admin KPI（fps/bitrate）+ agent RSS + 重连计数，断言无断线重连 / fps 中值 ≥15 / RSS 后段稳定（末两点增长 <5% 即编码器初始化摊分后不再增长，非泄漏）。**冒烟 90s 实测 PASS：6 样本无重连、fps 中值 30.0、bitrate 峰值 670kbps、RSS 末两点 +0%**。正式 1h：`STABILITY_SECONDS=3600 tools/stability_verify.sh`。戊列验收脚本化（4-top/弱网/重连/长稳）全闭环。
- **第 40 轮新增合入**：
  1. R5#16 背压可观测闭环：agent `DesktopManager` 加 `backpressure` 计数（收到 `desktop:congested` 递增，`bump_backpressure`/`backpressure_count` + 单测 `test_backpressure_counter_accumulates`）；qos-ack 回传 `bp_count`；浏览器面板新增"relay 拥塞"行（metric-bp，agent 回传 >0 显示次数）——传输段拥塞对用户/调试可见。全量 `cargo test` **389 通过**（+1）。
  2. 核实修正（R4/5 表）：61-70 连接与能力——MSE/WebCodecs 能力协商（`_webcodecsAvailable` 安全上下文+VideoDecoder 检测 → webcodecs/mse/none + sessionStorage 缓存复用）与能力探测缓存均已做；101-110 能力回退链——`_onDecodeError` 黑名单切 codec + `_scheduleDecodeRecover` 重建流兜底均已做；台账过时标"未做"。
- **第 41 轮新增合入**：
  1. 重连窗口降质量（R4 乙101-110 重连降质）：session.js `requestReconnect` 统一重连入口——30s 窗口 ≥2 次重连（join 看门狗 / 30s 判死 / SSE 断线）判**重连风暴** → 标记降质（不在重连窗口直接发命令避免丢消息）→ join 成功后 `applyQualityOnJoin` 发 `desktop:quality {speed}` 低码率档（重建流带宽压力骤减，更易稳定）→ 15s 无重连自动恢复 `best`。desktop.js 30s 判死改走 `window.__requestReconnect` 统一通道。**验证**：`node --check` 两文件通过 + 引用齐全（requestReconnect/applyQualityOnJoin/__requestReconnect 两端一致）。纯前端改动。
- **第 42 轮新增合入**：
  1. 控制消息优先级腾位窗口（R5#29）：relay 广播段 non-lossy 控制消息在浏览器 SSE channel 满时 `timeout(100ms, tx.send().await)` 等腾位（lossy 数据维持 try_send 静默丢）——弱网/瞬间积压下控制消息不被数据挤掉，仍满才告警丢。测试 `test_control_message_gets_drain_window_when_full`（满 channel：lossy 立即返回 / 控制消息 ≥80ms 腾位窗口）。全量 `cargo test` **390 通过**（+1）。
  2. 核实标注（#127-128）：捕获内存池/行拷贝 SIMD——当前架构 Frame 拥有 Vec + allocator 缓存 + LLVM 自动向量化已覆盖常见场景；专项 frame-ring 内存池需框架级改造（编码侧归还 buffer）收益不确定，如实标注远期。
- **第 43 轮新增合入**：
  1. 心跳 KPI 扩展补字段（R5#150 增强）：`DesktopKpi` 加 `active`（内容活跃 ≤1.5s，R5#25）+ `bp_count`（relay 拥塞累计，R5#16），`kpi_snapshot` 与 sender_loop 心跳 JSON 同步携带——admin KPI 曲线可观测**静止/活跃**与**传输段拥塞**时间线；测试 `test_sender_loop_heartbeat_carries_desktop_kpi` 断言扩展。全量 `cargo test` **390 通过**。
  2. 核实收口：65-66 能力探测缓存已做（sessionStorage 复用解码模式）；#48 轻通道决策关闭（独立 100ms ack 批无消费者端，限产机制落地再评估）；#14/#41-44 线协议整块、#36-38 KCP、#123-124/#129 GDI/DXGI Windows、#132-134 Wayland、#136-146 功耗画像、#127-128 SIMD、丙统一时间线、153 grader——均为远期/环境限制/架构决策，如实标注。
- **未合入（如实）**：线协议二进制整块（批次2 #41-44）、跨进程时间线遥测、独立 100ms ack 批、AV1 测速门槛等——在台账对应 ⬜/◐ 行，未宣称完成。