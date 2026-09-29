# shell-remote 抗弱网与鲁棒性分析（v0.54.0，MYS-1766）

本文梳理 shell-remote 三段链路在弱网 / 断线 / 服务重启下的行为，给出实测故障矩阵、本轮修复与剩余风险。行号以 v0.54.0 为准。

## 1. 链路拓扑

```
浏览器 ──SSE(GET /agent/session/sse) + POST(/agent/session/send)──► relay
relay  ──SSE(GET /agent/events)──────────────────────────────────► agent   （下行：输入/请求）
agent  ──POST(/agent/send) + WS(/agent/ws/send，仅桌面视频)────────► relay  （上行：输出/结果/心跳）
```

三段均为"长连接 SSE 下行 + 短请求 POST 上行"。任何一段的半开（TCP 未断但数据不通）都必须靠应用层超时发现，这是本分析的主线。

## 2. 现有机制清单（修复后）

| 层 | 机制 | 参数 | 位置 |
|---|---|---|---|
| relay→agent SSE | keep-alive 注释 | 5s | `src/relay/ws.rs:1402` |
| agent 下行判死 | SSE 空闲超时（按块） | 60s | `src/agent/client.rs:12` |
| agent 上行 | 每请求超时 **（新）** | 20s | `src/agent/client.rs:87` |
| agent 拨号 | connect_timeout + TCP keepalive **（新）** | 10s / 30s | `src/agent/client.rs:238` |
| agent 上行判死 | 心跳 POST 连续失败次数 **（新）** | 15s × 3 | `src/agent/mod.rs:222` |
| agent 注册重试 | 指数退避 + 抖动 [0.5x,1x]，429 固定 15s | 1s→60s，10 次 | `src/agent/client.rs:194`、`:448` |
| agent 会话重连 | 指数退避 + 抖动，健康会话（≥60s）后复位 **（新）** | 1s→60s（原 300s 不复位） | `src/agent/mod.rs:884` |
| agent 终端 | PTY 由进程级 `Terminals` 持有，跨重连存活 **（新）** | 每标签页回放 64KB | `src/agent/mod.rs:49`、`:56` |
| agent 上行缓冲 | 控制通道 64（背压）/ 输出通道 64（满则丢）+ 16ms 合并 | — | `src/agent/mod.rs:1080` |
| relay 事件回放 | agent SSE `Last-Event-ID` 回放缓冲 | 1000 条 / 8MB | `src/relay/mod.rs:194`、`:201` |
| relay→浏览器 | 每浏览器有界通道；lossy 丢、控制消息 100ms 腾位窗口 | 256 | `src/relay/mod.rs:25`、`src/relay/ws.rs:392` |
| relay 连接限流 | `/agent/events` + WS 上行共享配额，按 (IP, session) **（改）**，另有 per-IP 总配额 | 30/min；600/min | `src/relay/ws.rs:1288` |
| relay 桌面 viewer | 丢帧后重新等关键帧 **（新）**；连续丢 60 帧剔除 | 16 帧队列 | `src/relay/desktop.rs:58` |
| 浏览器 SSE | 空闲看门狗；指数退避 + 抖动 **（新）**；`online` / 回到前台立即重连 **（新）** | 30s；1s→10s | `web/sse.js:36`、`:122` |
| 浏览器鉴权失败 | 曾连上过则 401 容忍 90s 再退回登录页 **（新）** | 90s | `web/sse.js:29` |
| 浏览器输入 | 单飞行有序队列 + 合并 + 10s 超时 **（新）** | 单包 ≤64KB | `web/session.js:332` |

## 3. 故障矩阵（实测）

测试脚本见第 6 节。"旧版"= `origin/master`（v0.53.1，cd5c866），"新版"= 本分支。

### 3.1 终端会话续接（`tools/verify_weaknet_resume.js`，真实 relay + agent + 无头 Chromium）

| 故障 | 旧版 | 新版 |
|---|---|---|
| relay `kill -9` 后重启 | ✗ 浏览器被踢回登录页；agent 重连后 shell 被重建（PID 变化） | ✓ 5.3s 恢复输入，shell PID 不变 |
| agent↔relay 链路黑洞 70s（半开：连接不断、数据全丢） | ✗ agent 注册 POST 无超时，恢复网络后仍永久挂起，未重连 | ✓ 网络恢复后 13.5s 恢复输入，shell PID 不变 |
| 快速输入（无故障，本机） | ✗ 按键乱序：`echo ABC…xyz > /tmp/probe_order` 到达为 `…bacefdghijklnopmqrtsvuwyxz >t/m ppo/rbero_de_barse` | ✓ 原样到达 |

> 旧版跑该脚本时需设 `TYPE_DELAY_MS=80`，否则连初始输入都会被乱序打坏。

### 3.2 多 agent 并发注册（`tools/concurrent_register_smoke.sh`，40 agent）

| 场景 | 旧版 | 新版 |
|---|---|---|
| 40 个 agent 同时冷启动 | ✗ 60s 内仅 30/40 在线（共享 30/min SSE 配额） | ✓ 1s |
| relay `kill -9` 重启后 40 个同时重连 | 67s | ≤1s |
| 5 轮 × 10 个 agent 高频 kill/重启 | 69s 收敛 | ≤1s |
| 收敛后保持 20s | ✓ | ✓ |
| panic / 进程退出 | 无 | 无 |

## 4. 本轮修复的弱网 / 鲁棒性问题

按影响排序，均有单测或上面的端到端验证覆盖。

1. **agent 上行无任何超时（高）**：`reqwest::Client::new()` 无超时，半开链路下 POST 永久阻塞。心跳和输出随之停止，有界通道打满后主循环卡死；注册 POST 同样无超时，所以网络恢复后也不再重连（3.1 旧版黑洞场景）。修复：连接超时、TCP keepalive、每个 POST 20s 超时。
2. **上行半开无法判死（高）**：只有下行 SSE 空闲能触发重连。修复：心跳 POST 连续失败 3 次即结束会话、重新注册（`sender_loop` → `Notify`）。
3. **每次重连都杀掉全部 shell（高）**：`run_session` 结束时 `tabs.clear()`，一次网络抖动就会丢掉用户正在跑的命令。重建的 shell 标签页 id 也会变，浏览器仍持有旧 `activeTabId`，于是输出被过滤、输入发往不存在的标签页，终端表现为"假死"直到刷新页面。修复要点：
   - PTY 改由 `start()` 持有的 `Terminals` 管理，跨会话存活。TUI 手动刷新 token 属于撤销访问，这种情况仍会结束全部 shell，不把运行中的会话和回放历史交给新 token 持有者。
   - 断线期间 shell 输出持续写入回放缓冲，不堆积在无界通道里。
   - shell 退出时回收该标签页；最后一个标签页退出时重建一个 shell。
   - 浏览器在 `tab_list` 中找不到当前标签页时自动切换。
4. **浏览器输入乱序（高）**：每个按键一个并发 fetch，HTTP/1.1 多连接或 HTTP/2 多路复用都不保证到达顺序，本机即可复现。修复：同一时刻最多一个 POST 在途，在途期间的按键合并到下一包，UTF-8 安全 base64 分块编码（修复大段粘贴 `btoa(...spread)` 的 RangeError）。
5. **relay 重启把用户踢回登录页（中）**：新 relay 在 agent 重新注册前不认识 token，返回 401，前端立即跳转 `/`。修复：页面曾经连上过则容忍 90s 并提示"正在重试"。从未连上（密钥填错）仍立即返回登录页。
6. **只读用户输入被踢回登录页（中）**：`/agent/session/send` 对只读用户的写消息返回 403，旧前端把 403 当作鉴权失败处理并跳转。修复：只读用户的按键在本地丢弃并提示"只读访问"（限频），不再发往 relay。
7. **共享 SSE 连接配额引发重连雪崩（中）**：`ev:` 配额仅按 IP 计。无 `X-Forwarded-For` 时所有 agent 落入 `"unknown"`，同一 NAT 后的 agent 也会共享配额；>30 个 agent 同时重连时超出部分的 SSE 被 429，形成反复重连（3.2）。修复：key 改为 (IP, session)，单个 agent 仍受 30/min 约束；session 由调用方提供，所以另设 per-IP 600/min 总配额，防止伪造 session 绕过限流；限流表超过阈值时清理过期 key（阈值随之翻倍，均摊 O(1)），防止无界增长。
8. **会话重连退避永不复位（中）**：`start()` 的 `delay` 只在 TUI 刷新时复位，进程生命周期内累计约 9 次断线后，每次重连都要等 5 分钟。修复：会话存活 ≥60s 视为健康，退避复位到 1s，上限 60s，并加抖动。
9. **SSE 多字节字符跨块被破坏（中）**：`from_utf8_lossy` 按网络块解码，弱网小分段时中文输入 / 文件内容会变成 U+FFFD。修复：按字节缓冲，仅对完整事件解码。
10. **桌面 viewer 丢帧后花屏（中）**：队列满跳过一帧后仍继续投递后续差分帧，参考链已断，画面花屏直到下一个 IDR。修复：丢帧后该 viewer 重新等待关键帧（心跳 IDR 1.5–4.5s）。接收端已关闭的 viewer 立即剔除；等待关键帧期间队列仍满也计入剔除阈值。
11. **其它**：
    - 下行 SSE 非 2xx（如 relay 重启后的 404/429）立即结束会话。
    - 会话结束时回收 SSE 读任务与 sender 任务，不再泄漏连接。
    - 下载分块 POST 失败时中止下载，不再静默产生空洞文件（relay 不去重，故不重试）。
    - 升级下载 body 60s 无数据判失败，否则 `upgrade_in_progress` 永久置位。
    - 以下几处 panic 已修复：`exec_sessions` 输出缓冲按字节 drain 遇多字节字符；relay 日志截取 token 前缀遇非 ASCII `--key`；agent 截取响应体日志。
    - `constant_time_eq` 不再泄漏密钥长度。
    - 浏览器 join 时的输出回放只发给加入者，其它在线用户不再看到重复输出。
    - SSE handler 抛错不再杀掉整个 SSE 读循环。

## 5. 剩余风险与建议（未在本轮处理）

| 优先级 | 问题 | 说明 / 建议 |
|---|---|---|
| 高（安全） | agent 通道无认证 | 以下接口都不校验服务器密码：`agent:register`、已知 session 的 `/agent/send`、`/agent/events`。知道或猜到 `session_id` 的人可以接管会话、读取终端输入（`ws.rs` register / events handler）。建议 register 时下发 per-session agent secret，后续 events / send / ws 上行都校验它。该改动涉及协议兼容（老 agent），需单独设计。 |
| 中 | 限流 IP 取自可伪造的 `X-Forwarded-For` | 直连部署时客户端可以伪造该头绕过限流。建议无可信反代时使用 socket 对端地址（`ConnectInfo`），有反代时只信任最后一跳。 |
| 中 | relay 侧半开检测依赖 TCP | relay 的 agent SSE 与浏览器 SSE 都没有应用层超时，靠写失败发现断连。实测半开期间 relay 要等 agent 侧 60s 判死重连后才更新状态。WS 上行的 20s Ping 没有 Pong 截止判断，但 agent 从不读该 socket，直接加 Pong 截止会误杀正常连接；需要先让 agent 读取并回 Pong。 |
| 中 | 重连窗口内的消息不保证送达 | 浏览器 POST 在 agent 缺席或队列满时仍返回 202（`deliver` 静默丢弃）；agent 端未使用 `Last-Event-ID`。终端输入有前端失败提示，MCP / 文件请求依赖调用方超时。建议缺席时返回 503，让前端可以重试或提示。 |
| 低 | 会话状态清理不完整 | reaper / 管理员踢除只清理部分 per-session 映射（`desktop_proto`、`kpi_history` 等残留）。建议统一实现 `remove_session_state()`。 |
| 低 | `exec_sessions` stdin 写入与读循环同处一个 `select!` | 子进程不读 stdin 且输出很多时可能互相阻塞。建议 stdin 写入独立成任务。 |
| 低 | 文件操作未限制在 `--root` 内 | 与现有测试预期一致（允许 `..`），属设计选择；如需沙箱，应在 canonicalize 后校验前缀。 |
| 低 | Windows 上 shell 退出检测 | ConPTY 读端在子进程退出后未必 EOF，退出的标签页不会被自动回收（与旧版一致）。 |

## 6. 复现与回归

```bash
# 单元 / 集成测试（lean 与全特性；全特性的 X11 采集测试需 Xvfb :99）
cargo test --no-default-features
Xvfb :99 & cargo test

# 终端续接端到端（relay kill -9 + 半开黑洞 70s），需 playwright + chromium
cargo build --no-default-features
BIN=./target/debug/shell-remote node tools/verify_weaknet_resume.js

# 多 agent 高频并发注册冒烟（40 agent：冷启动 / relay 重启 / 5×10 抖动 / 20s 保持）
BIN=./target/debug/shell-remote tools/concurrent_register_smoke.sh
```
