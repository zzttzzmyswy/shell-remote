# 性能、防堵塞与弱网鲁棒性

## 防堵塞：多层隔离

为避免单个大文件传输或大量终端日志堵塞其他会话，relay 做了多层隔离：

- **文件分块传输** — 上传/下载按 256KB 切成小块消息流式收发（`src/relay/file_transfer.rs` 的 `CHUNK_SIZE`），单条消息永远不大，不会被一条巨型消息占住 worker 线程或撑爆内存
- **有界 channel** — relay→agent、relay→browser 的 SSE 通道均有界（256 条，`src/relay/mod.rs` 的 `SSE_CHANNEL_CAPACITY`）。满时优先丢弃可丢失的终端输出帧（`terminal:output` 等），保留控制/结果消息，确保一个卡住的消费者不会无限制涨内存拖垮整个 relay 进程
- **EventBuffer 字节上限** — 会话事件回放缓冲除条数上限（1000 条）外再加 8MB 字节上限（`MAX_EVENT_BUFFER` / `MAX_EVENT_BUFFER_BYTES`），防止大消息或持续日志刷爆回放缓存
- **背压** — 上传分块走背压发送（agent 跟不上时等待而非丢帧）；下载在独立任务中流式发送，不阻塞 agent 主循环的终端输入转发
- **带宽限制** — relay 整体上下行可在后台在线设定，文件传输/升级包等待、桌面视频丢帧降码率、交互消息只记账不等待。见 [admin.md](admin.md)
- **消息体积上限** — 浏览器发往 relay 的单条控制消息上限 8MB，超出返回 `413`

代价：某个会话的消费者严重卡住时，该会话的传输可能失败/丢帧，但不会影响其他会话的响应。

## 弱网续接

终端 shell（PTY）跨 relay 重连存活：断网 / relay 重启 / 半开链路恢复后，正在运行的命令不中断，浏览器重新加入时回放各标签页最近 64KB 输出（`src/agent/mod.rs` 的 `TAB_REPLAY_BYTES`）。

| 机制 | 参数 | 位置 |
|------|------|------|
| relay→agent SSE keep-alive 注释 | 5s | `src/relay/ws.rs` |
| agent 下行判死：SSE 空闲超时 | 60s | `src/agent/client.rs` 的 `AGENT_SSE_IDLE_TIMEOUT` |
| agent 上行：每个 POST 超时 | 20s | `src/agent/client.rs` |
| agent 拨号：connect_timeout + TCP keepalive | 10s / 30s | `src/agent/client.rs` |
| agent 上行判死：心跳 POST 连续失败 | 15s × 3 次 | `src/agent/mod.rs` 的 `UPLINK_DEAD_AFTER_HEARTBEAT_FAILURES` |
| agent 注册重试：指数退避 + 抖动 | 1s→60s，最多 10 次 | `src/agent/client.rs` |
| agent 会话重连：指数退避 + 抖动，健康会话后复位 | 1s→60s | `src/agent/mod.rs` |
| relay 事件回放 | 1000 条 / 8MB | `src/relay/mod.rs` |
| relay→浏览器有界通道 | 256 条 | `src/relay/mod.rs` |
| 浏览器 join 应答看门狗 | 5s 无应答自动重连 | `web/session.js` 的 `JOIN_ACK_TIMEOUT` |
| 浏览器 SSE 空闲看门狗 / 退避 | 30s；1s→10s | `web/sse.js` |
| 浏览器鉴权失败容忍（曾连上过） | 90s | `web/sse.js` |
| 浏览器输入有序队列 | 单飞行 + 合并，单包 ≤64KB | `web/session.js` |
| 重连风暴降质 | 30s 内 ≥2 次重连即降码率，稳定 15s 后恢复 | `web/session.js` |

### 连接自愈与提示

设备已注册但 agent 链路未建立或断开时，浏览器不会僵死在空白终端：

- 终端页提示"设备连接中断，正在自动重连…"并自动重连，agent 恢复后终端回放续上
- 若 join 因 agent 通道失效（重连/顶替窗口）无法送达，relay 通知浏览器重连
- join 发出后 5s 内无任何控制事件回传（`session:users` / `session:tab_list` / 终端输出）→ 判定 join 静默丢失，提示并自动重连——任何链路静默丢失都不会留下永久的空白终端
- 页面曾连上过时，relay 重启窗口内的 401 被容忍 90s 并提示"正在重试"；从未连上（密码填错）仍立即回登录页
- 只读 Token 的按键在本地丢弃并提示"只读访问"（限频），不再发往 relay，也不会被 403 误判为鉴权失败而跳转登录页

## 桌面链路的 QoS

- **内容驱动帧率**：静态桌面降到 1fps，有内容立即拉满；网络/延时退出帧率决策，帧率只由内容活动与解码背压决定
- **弱网优先降质量而非掉帧**：内容不变时无法区分动态与静止，所以码率优先于帧率
- **丢帧后重新等关键帧**：viewer 队列满跳过一帧后，参考链已断，relay 让该 viewer 重新等待关键帧（心跳 IDR），避免花屏；连续丢 60 帧剔除该 viewer
- **拥塞回传**：relay→浏览器 fan-out 丢旧保新发生时向 agent 回传 `desktop:congested`（≥5s / 次限频），agent 自动降码率

## 详细故障矩阵

逐条故障场景、旧版/新版对比与修复清单（v0.54.0 基线，行号以该版本为准）见 [weak-network-robustness.md](weak-network-robustness.md)。该文档同时列出了尚未处理的剩余风险（agent 通道无认证、限流 IP 取自可伪造的 `X-Forwarded-For`、relay 侧半开检测依赖 TCP、重连窗口内消息不保证送达等）。

## 回归验证

```bash
cargo test --no-default-features
```

涉及连接 / 会话 / 限流改动的发布前必须做多 agent 高频并发注册冒烟：

```bash
cargo build --no-default-features
BIN=./target/debug/shell-remote tools/concurrent_register_smoke.sh
```

终端续接的端到端验证（relay `kill -9` + 半开黑洞 70s，需 playwright + chromium）：

```bash
BIN=./target/debug/shell-remote node tools/verify/verify_weaknet_resume.js
```

其余压测与测量脚本在 `tools/`（如 `stability_verify.sh`、`weaknet_matrix.sh`、`reconnect_matrix.sh`、`multi_session_isolation.sh`）与 `tools/bench/`，用法见各脚本头部注释。
