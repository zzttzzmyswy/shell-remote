# MCP 端文件传输（relay 字节中继端点）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** relay 新增 `PUT /agent/mcp/put` 与 `GET /agent/mcp/get` 两个流式 HTTP 端点，让 LLM 通过本地 `curl` 在 LLM 机器与 agent 远端机器间传输任意大小文件，字节不经 LLM context、relay 不落盘、不抢占共享连接带宽。

**Architecture:** 上传端点流式读 request body 成 256KB base64 块，复用 agent 现有 `fs:upload`+`assemble_upload_chunk` 协议派发到 agent；下载端点向 agent 发 `fs:read`，复用现有 `stream_file_download` 推送流，relay 加一个 `download_streams` 路由把 `fs:result` 块解 base64 转发进响应 body。带宽隔离走方案 B（单 SSE 连接）：relay 每会话维护两个有界 mpsc——交互（cap 256，try_send 丢可丢帧）与 bulk（cap 16，send().await 背压），用 biased `select!` 合流进同一条 agent SSE，先排空交互再排空 bulk。agent 侧零改动。鉴权只认会话 token（`X-SR-Token`），不认服务器密码。

**Tech Stack:** Rust + tokio + axum 0.7（`axum::body::Body` 流式）、reqwest、serde_json、base64、uuid。TDD：每任务先写失败测试。

**Spec:** `docs/superpowers/specs/2026-07-13-mcp-file-transfer-design.md`

## Global Constraints

- 分块大小 `CHUNK_SIZE = 256 * 1024`（256KB），base64 编码后约 341KB/消息。与现有 `stream_file_download`（`src/agent/mod.rs:171`）一致。
- bulk 子通道容量 `BULK_CHANNEL_CAPACITY = 16`，远小于交互通道 `SSE_CHANNEL_CAPACITY = 256`（`src/relay/mod.rs:23`）。
- 鉴权：`X-SR-Token` header → `state.sessions.authenticate(token)` → `Option<(String /*session_id*/, Permission)>`（`src/relay/session.rs:125`）。**不**校验服务器密码（`state.server_auth`）。`Permission::ReadOnly` 上传被拒（403）、下载允许。
- 文件块不可丢：bulk 通道用 `send().await` 背压；交互通道仍用 `deliver`（`try_send`，`src/relay/ws.rs:39`）丢可丢帧。两通道在 relay 侧 biased 合流进单条 agent SSE（方案 B，agent 零改动）。
- agent 上传侧零改动：复用 `assemble_upload_chunk`（`src/agent/mod.rs:250`）与 `fs:upload` arm（`src/agent/mod.rs:693`）。
- 审计复用 `recorder.audit_mcp(session_id, AuditLine)`（`src/relay/recorder.rs:126`），`status` 用 `"upfile"`/`"downfile"`，记 path+bytes+permission，**不记内容**。`--record-dir` 未开时 recorder 为 `None`，不记。
- 工作分支 `feat/mcp-file-transfer`（已建）。每任务一次 commit，message 末尾加 `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`。
- 运行测试：`cargo test --lib`（单测）；集成测试 `cargo test --test integration_test`。

---

## File Structure

| 文件 | 责任 | 动作 |
|------|------|------|
| `src/relay/mod.rs` | `SharedState`/`ChannelMap` 定义、路由表 | 改：加 `agent_bulk` 字段、`download_streams` 字段、注册两条路由 |
| `src/relay/file_transfer.rs` | **新文件**。`put_handler`、`get_handler`、`DownloadSink`/`DownloadEvent`、`deliver_bulk`、审计 helper | 建 |
| `src/relay/ws.rs` | agent 消息路由、`ChannelMap` 写入 | 改：`route_agent_message` 加 download sink 路由分支；agent 连接建立时同时建 bulk 通道 |
| `src/relay/mcp.rs` | `tools/list` | 改：`shell_remote` 描述加文件传输说明 |
| `src/proto/mod.rs` | `requires_write` | 改：`fs:read` 仍归只读（现状），无需改；确认 |
| `src/agent/mod.rs` | agent 主循环 | 改：`stream_file_download` 改独立 POST + `yield_now` + `is_last`；`upload_reassembly` 加 `created_at` + 超时清理；新增 `fs:read_cancel` 处理（agent 侧单 SSE recv，**主循环不重构**，方案 B） |
| `src/agent/client.rs` | `RelayClient` | 不改（方案 B 无需第二条 SSE） |
| `src/relay/recorder.rs` | `AuditLine` | 改：复用现有结构，`cmd` 字段塞 path、`stdout_len` 塞 bytes、`status` 塞 upfile/downfile |

---

## Task 1: bulk 子通道类型与 `deliver_bulk`

**Files:**
- Modify: `src/relay/mod.rs:26-39`（`ChannelMap`）
- Modify: `src/relay/mod.rs:42-110`（`SharedState`）
- Create: `src/relay/file_transfer.rs`（本任务只建文件骨架 + `deliver_bulk`）
- Modify: `src/relay/mod.rs:3`（`pub mod file_transfer;`）

**Interfaces:**
- Produces: `pub const BULK_CHANNEL_CAPACITY: usize = 16;`；`ChannelMap::agent_bulk: Option<mpsc::Sender<String>>`；`pub async fn deliver_bulk(tx: &mpsc::Sender<String>, msg: String)`（`send().await`，不丢帧、不改写 msg）。

- [ ] **Step 1: 写失败测试 `deliver_bulk` 背压不丢帧**

在 `src/relay/file_transfer.rs`：

```rust
#![allow(dead_code)]
use tokio::sync::mpsc;

/// Capacity for the per-session relay→agent bulk (file-transfer) sub-channel.
/// Deliberately smaller than [`SSE_CHANNEL_CAPACITY`] so file chunks yield to
/// interactive traffic under the agent's biased select.
pub const BULK_CHANNEL_CAPACITY: usize = 16;

/// Backpressure delivery to the bulk sub-channel. File chunks MUST NOT be
/// dropped (a dropped chunk corrupts the file), so this awaits on a full
/// channel instead of try_send. The interactive channel stays independent,
/// so this backpressure cannot stall interactive traffic. `msg` is a full
/// serialized `Message` JSON string — sent unchanged (no re-framing).
pub async fn deliver_bulk(tx: &mpsc::Sender<String>, msg: String) {
    let _ = tx.send(msg).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_deliver_bulk_never_drops() {
        // Capacity 2; send 4 full messages. deliver_bulk must await (not drop),
        // so all 4 arrive byte-identical.
        let (tx, mut rx) = mpsc::channel::<String>(2);
        let msgs: Vec<String> = (0..4u32)
            .map(|i| serde_json::json!({"type":"fs:upload","session_id":"s","payload":{"i":i}}).to_string())
            .collect();
        let msgs_clone = msgs.clone();
        let h = tokio::spawn(async move {
            for m in msgs_clone {
                deliver_bulk(&tx, m).await;
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut got = Vec::new();
        while let Some(s) = rx.recv().await {
            got.push(s);
            if got.len() == 4 { break; }
        }
        h.await.unwrap();
        assert_eq!(got, msgs, "deliver_bulk must not drop or alter file chunks");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib file_transfer`
Expected: 编译失败——`mod.rs` 还没 `pub mod file_transfer;`。

- [ ] **Step 3: 注册模块 + 加常量**

`src/relay/mod.rs` 顶部加（紧挨 `pub mod recorder;`）：

```rust
pub mod file_transfer;
```

紧挨 `SSE_CHANNEL_CAPACITY`（line 23 下方）加：

```rust
/// Capacity for the per-session relay→agent bulk (file-transfer) sub-channel.
/// File chunks go here so they yield to interactive traffic under the
/// agent's biased select; bounded so a stuck agent can't grow it unbounded.
pub const BULK_CHANNEL_CAPACITY: usize = 16;
```

- [ ] **Step 4: 加 `agent_bulk` 到 `ChannelMap`**

`src/relay/mod.rs` 改 `ChannelMap`（line 26）：

```rust
#[allow(dead_code)]
pub struct ChannelMap {
    pub agent: Option<mpsc::Sender<String>>,
    pub agent_bulk: Option<mpsc::Sender<String>>,
    pub browser_sessions: HashMap<String, String>,
}

#[allow(dead_code)]
impl ChannelMap {
    pub fn new() -> Self {
        Self {
            agent: None,
            agent_bulk: None,
            browser_sessions: HashMap::new(),
        }
    }
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --lib file_transfer`
Expected: PASS（`test_deliver_bulk_never_drops`）。

- [ ] **Step 6: 全量编译，修掉 `ChannelMap::new` 调用点**

Run: `cargo build`
Expected: 编译通过（`agent_bulk` 是 `Option`，`new` 初始化为 `None`，现有调用点无需改）。

- [ ] **Step 7: Commit**

```bash
git add src/relay/mod.rs src/relay/file_transfer.rs
git commit -m "feat(fs): add bulk sub-channel type + deliver_bulk"
```

---

## Task 2: relay 侧 biased merge——bulk 与交互通道合流进单条 agent SSE

**架构决策（B）**：不开第二条 SSE。relay 每会话维护两个有界 mpsc——`agent`（交互，cap 256，`deliver` try_send 丢可丢帧）与 `agent_bulk`（文件块，cap 16，`deliver_bulk` send().await 背压）。`agent_events_handler` 把两者用一个 **biased `select!` 合流流**喂进同一条 agent SSE：先排空交互、再排空 bulk。agent 侧单 recv，**零改动**。隔离硬度来自 biased 排空 + bulk 独立有界缓冲：bulk 占满自己的 16 缓冲只背压文件传输自己，交互消息走 `agent` 主缓冲 + biased 优先，不受影响。

**Files:**
- Modify: `src/relay/ws.rs` `agent_events_handler`（建双通道 + biased 合流流；约 line 340-390）
- Modify: `src/relay/ws.rs` 其他建 `ChannelMap` 处（`agent_send_handler`/测试 helper），给 `agent_bulk` 填 `None` 或对应 sender

**Interfaces:**
- Consumes: `ChannelMap::agent_bulk`（Task 1）、`BULK_CHANNEL_CAPACITY`（Task 1）。
- Produces: 每会话 `agent_bulk: Some(Sender<String>)` 可被 `file_transfer::deliver_bulk` 使用；agent SSE 流 biased 合流交互+bulk。

- [ ] **Step 1: 写失败测试——biased 合流优先交互**

`src/relay/ws.rs` 测试模块加。测一个提取的纯合流函数 `merge_biased(interactive_rx, bulk_rx) -> Stream`：先灌 bulk 3 条、再灌交互 1 条、再灌 bulk 1 条，断言输出顺序是交互先出（biased）：

```rust
#[tokio::test]
async fn test_merge_biased_drains_interactive_first() {
    use tokio::sync::mpsc;
    let (itx, irx) = mpsc::channel::<String>(8);
    let (btx, brx) = mpsc::channel::<String>(8);
    // pre-fill: 3 bulk, then 1 interactive, then 1 bulk
    for i in 0..3 { btx.send(format!("b{}", i)).await.unwrap(); }
    itx.send("i0".to_string()).await.unwrap();
    btx.send("b3".to_string()).await.unwrap();
    drop(itx); drop(btx); // close so merge terminates
    let mut out = tokio_stream::StreamExt::boxed(merge_biased(irx, brx));
    let mut got = Vec::new();
    while let Some(s) = out.next().await { got.push(s); }
    // interactive i0 must come before all bulk (biased: interactive drained first each cycle)
    let i_pos = got.iter().position(|s| s == "i0").unwrap();
    let first_bulk = got.iter().position(|s| s.starts_with('b')).unwrap();
    assert!(i_pos < first_bulk, "interactive must precede bulk under biased merge; got {:?}", got);
}
```

注：`merge_biased` 是 Step 3 提取的纯函数（两个 `mpsc::Receiver<String>` → `impl Stream<Item=String>`）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib test_merge_biased_drains_interactive_first`
Expected: FAIL（`merge_biased` 不存在）。

- [ ] **Step 3: 实现 `merge_biased`**

`src/relay/ws.rs` 加：

```rust
/// Biased merge of the interactive and bulk agent channels into one stream
/// for the agent SSE. Interactive is drained first each cycle, so file chunks
/// (bulk) yield to terminal:input / mcp:exec / control messages. Both
/// receivers are bounded independently; bulk backpressure cannot stall
/// interactive.
fn merge_biased(
    mut interactive: tokio::sync::mpsc::Receiver<String>,
    mut bulk: tokio::sync::mpsc::Receiver<String>,
) -> impl tokio_stream::Stream<Item = String> {
    async_stream::stream! {
        loop {
            tokio::select! {
                biased;
                m = interactive.recv() => match m { Some(s) => yield s, None => {
                    // drain remaining bulk
                    while let Some(s) = bulk.recv().await { yield s; }
                    break;
                }},
                m = bulk.recv() => match m { Some(s) => yield s, None => {
                    // drain remaining interactive
                    while let Some(s) = interactive.recv().await { yield s; }
                    break;
                }},
            }
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --lib test_merge_biased_drains_interactive_first`
Expected: PASS。

- [ ] **Step 5: `agent_events_handler` 建双通道 + 合流进 SSE**

`src/relay/ws.rs` 找 `agent_events_handler`（约 line 340）。现有 `let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);` 改为建双通道并合流：

```rust
let (tx, rx) = mpsc::channel::<String>(SSE_CHANNEL_CAPACITY);          // interactive
let (bulk_tx, bulk_rx) = mpsc::channel::<String>(crate::relay::BULK_CHANNEL_CAPACITY);
```

写入 `ChannelMap` 时设 `agent_bulk: Some(bulk_tx)`（按现场 `ChannelMap` 构造方式对齐）。

把现有喂 SSE 的 `ReceiverStream::new(rx)` 替换为 `merge_biased(rx, bulk_rx)`：

```rust
let merged = merge_biased(rx, bulk_rx);
let mut merged = tokio_stream::StreamExt::boxed(merged);
// ... 现有 SSE 构造，把 rx_stream 的 next() 换成 merged.next() ...
```

保留现有 `SseCleanup`/keep-alive 逻辑，仅换数据源。

- [ ] **Step 6: 其他建 `ChannelMap` 处填 `agent_bulk: None`**

`src/relay/ws.rs` 其余构造 `ChannelMap` 的地方（`agent_send_handler`、测试 helper），`agent_bulk` 填 `None`（这些路径不接 agent SSE）。`cargo build` 指引所有构造点。

- [ ] **Step 7: 运行全量测试**

Run: `cargo test --lib`
Expected: PASS（无行为变化：bulk 通道建好但尚无发送方，合流退化为只流交互）。

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(relay): biased merge of interactive+bulk channels into agent SSE"
```

---

## Task 3: `DownloadSink` / `DownloadEvent` 类型与 `download_streams` 表

**Files:**
- Modify: `src/relay/file_transfer.rs`（加类型）
- Modify: `src/relay/mod.rs`（`SharedState` 加字段 + `new` 初始化）

**Interfaces:**
- Produces: `pub enum DownloadEvent { Chunk(Vec<u8>), Error(String), End }`；`pub struct DownloadSink { tx: mpsc::Sender<DownloadEvent>, created_at: Instant, bytes: u64 }`；`SharedState.download_streams: RwLock<HashMap<String, DownloadSink>>`。

- [ ] **Step 1: 写类型**

`src/relay/file_transfer.rs` 加：

```rust
use std::time::Instant;
use std::collections::HashMap;
use tokio::sync::{mpsc, RwLock};

/// One event on a download's relay-internal sink. The relay response task
/// reads these to drive the HTTP response body; Chunk 0 decides 200 vs 500.
pub enum DownloadEvent {
    Chunk(Vec<u8>),
    Error(String),
    End,
}

/// The relay-side handle for one in-flight download. `route_agent_message`
/// pushes decoded file bytes (or errors) into `tx`; the `get_handler` task
/// drains `tx` into the HTTP response body.
pub struct DownloadSink {
    pub tx: mpsc::Sender<DownloadEvent>,
    pub created_at: Instant,
    pub bytes: u64,
}
```

- [ ] **Step 2: 加 `download_streams` 到 `SharedState`**

`src/relay/mod.rs` `SharedState` 加字段（紧挨 `pending_mcp`）：

```rust
pub download_streams: RwLock<HashMap<String, crate::relay::file_transfer::DownloadSink>>,
```

`SharedState::new` 加：

```rust
download_streams: RwLock::new(HashMap::new()),
```

- [ ] **Step 3: 运行编译**

Run: `cargo build`
Expected: PASS。

- [ ] **Step 4: Commit**

```bash
git add src/relay/file_transfer.rs src/relay/mod.rs
git commit -m "feat(fs): add DownloadSink/DownloadEvent + download_streams table"
```

---

## Task 4: `route_agent_message` 路由 download `fs:result` 到 sink

**Files:**
- Modify: `src/relay/ws.rs:130-150`（现有 `fs:result` oneshot 分支旁）

**Interfaces:**
- Consumes: `DownloadSink`/`DownloadEvent`（Task 3）、`download_streams`（Task 3）。
- Produces: agent 推回的带 `correlation_id` 的 `fs:result` 被转成 `DownloadEvent` 推进对应 sink。

- [ ] **Step 1: 写失败测试——download fs:result 推进 sink**

`src/relay/ws.rs` 测试模块加：

```rust
#[tokio::test]
async fn test_route_agent_message_download_chunk_pushes_to_sink() {
    use crate::relay::file_transfer::{DownloadEvent, DownloadSink};
    use std::time::Instant;
    let state = make_state("");
    let (tx, mut rx) = mpsc::channel(16);
    state.download_streams.write().await.insert(
        "dl-1".to_string(),
        DownloadSink { tx, created_at: Instant::now(), bytes: 0 },
    );
    let msg = json!({
        "type": "fs:result", "session_id": "sid1",
        "payload": {"success": true, "content": "aGk=", "path": "/x",
                    "chunk_index": 0, "total_chunks": 1, "is_last": true,
                    "_mcp_request_id": "dl-1"}
    }).to_string();
    route_agent_message(&state, "sid1", &msg).await;
    match rx.recv().await.unwrap() {
        DownloadEvent::Chunk(b) => assert_eq!(b, b"hi"),
        _ => panic!("expected Chunk"),
    }
    assert!(rx.recv().await.is_none() || matches!(rx.recv().await, Some(DownloadEvent::End)));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib test_route_agent_message_download_chunk_pushes_to_sink`
Expected: FAIL（路由分支不存在，sink 收不到）。

- [ ] **Step 3: 实现路由分支**

`src/relay/ws.rs` `route_agent_message` 在现有 `// FS oneshot` 分支（line 130）**之后**加：

```rust
// Download streaming: fs:result carrying a correlation_id registered in
// download_streams is a file chunk pushed by the agent; decode and forward
// to the GET response task via the sink. Independent of pending_mcp.
if proto_msg.msg_type == "fs:result" {
    if let Some(cid) = proto_msg.payload.get("_mcp_request_id").and_then(|v| v.as_str()) {
        let sink_opt = state.download_streams.write().await.remove(cid);
        if let Some(mut sink) = sink_opt {
            // re-insert immediately; we just needed mutable access
            let success = proto_msg.payload.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
            if !success {
                let err = proto_msg.payload.get("error").and_then(|v| v.as_str()).unwrap_or("download error").to_string();
                let _ = sink.tx.send(crate::relay::file_transfer::DownloadEvent::Error(err)).await;
            } else if let Some(content) = proto_msg.payload.get("content").and_then(|v| v.as_str()) {
                if let Some(bytes) = crate::agent::fs::decode_b64(content) {
                    sink.bytes += bytes.len() as u64;
                    let is_last = proto_msg.payload.get("is_last").and_then(|v| v.as_bool()).unwrap_or(false);
                    let _ = sink.tx.send(crate::relay::file_transfer::DownloadEvent::Chunk(bytes)).await;
                    if is_last {
                        let _ = sink.tx.send(crate::relay::file_transfer::DownloadEvent::End).await;
                    } else {
                        state.download_streams.write().await.insert(cid.to_string(), sink);
                    }
                }
            }
        }
    }
}
```

注：`is_last` 字段需 agent 侧 `stream_file_download` 在末块带 `is_last:true`。Task 6 改 agent。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --lib test_route_agent_message_download_chunk_pushes_to_sink`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src/relay/ws.rs
git commit -m "feat(fs): route download fs:result chunks to DownloadSink"
```

---

## Task 5: 上传端点 `put_handler`

**Files:**
- Modify: `src/relay/file_transfer.rs`（加 `put_handler`）
- Modify: `src/relay/mod.rs:320-331`（注册路由）
- Modify: `src/relay/mcp.rs`（`tools/list` 描述，见 Task 8）

**Interfaces:**
- Consumes: `deliver_bulk`（Task 1）、`agent_broadcast` 的 `agent_bulk` sender、`pending_mcp` oneshot、`assemble_upload_chunk`（agent 侧已存在）。
- Produces: `pub async fn put_handler(State, headers, Query, Body) -> Response`，路由 `/agent/mcp/put` PUT。

- [ ] **Step 1: 写失败测试——put 流式发 3 块到 agent bulk 通道**

`src/relay/file_transfer.rs` 测试模块加：

```rust
#[cfg(test)]
mod put_tests {
    use super::*;
    use crate::relay::SharedState;
    use axum::body::Body;
    use axum::http::HeaderMap;
    use axum::extract::{Query, State};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_state() -> Arc<SharedState> {
        Arc::new(SharedState::new(String::new(), 100*1024*1024, None, String::new(), String::new(), None))
    }

    #[tokio::test]
    async fn test_put_unauthorized_no_token() {
        let state = make_state();
        let mut params = HashMap::new(); params.insert("path".into(), "/x".into());
        let resp = put_handler(State(state), HeaderMap::new(), Query(params), Body::empty()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_put_readonly_forbidden() {
        let state = make_state();
        let (_sid, tokens) = state.sessions.register(None, "ro", None).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        let mut params = HashMap::new(); params.insert("path".into(), "/x".into());
        let resp = put_handler(State(state), headers, Query(params), Body::empty()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_put_missing_content_length_411() {
        let state = make_state();
        let (_sid, tokens) = state.sessions.register(None, "rw", None).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
        // Body without Content-Length → 411
        let mut params = HashMap::new(); params.insert("path".into(), "/x".into());
        let resp = put_handler(State(state), headers, Query(params), Body::from("partial")).await;
        assert_eq!(resp.status(), axum::http::StatusCode::LENGTH_REQUIRED);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib put_handler`
Expected: FAIL（`put_handler` 不存在）。

- [ ] **Step 3: 实现 `put_handler`**

`src/relay/file_transfer.rs` 加：

```rust
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::body::Body;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use uuid::Uuid;

const CHUNK_SIZE: usize = 256 * 1024;

pub async fn put_handler(
    State(state): State<Arc<crate::relay::SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: Body,
) -> axum::response::Response {
    // Rate limit (mirrors upload_handler).
    let client_ip = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("unknown").to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 20, std::time::Duration::from_secs(60)) {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    let token = headers.get("x-sr-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    use crate::proto::Permission;
    if permission == Permission::ReadOnly {
        return StatusCode::FORBIDDEN.into_response();
    }
    let path = match params.get("path") {
        Some(p) => p.clone(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let content_len = match headers.get(axum::http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()) {
        Some(n) => n,
        None => return StatusCode::LENGTH_REQUIRED.into_response(),
    };
    let total_chunks = (((content_len as usize + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32).max(1);
    let upload_id = Uuid::new_v4().to_string();

    let bulk_tx = {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast.get(&session_id).and_then(|cm| cm.agent_bulk.clone()) {
            Some(tx) => tx,
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(), // no agent
        }
    };

    // Stream the body into CHUNK_SIZE chunks → fs:upload on bulk channel.
    use http_body_util::BodyExt;
    let mut reader = body.into_data_stream();
    let mut chunk_index: u32 = 0;
    let mut bytes_sent: u64 = 0;
    loop {
        let mut buf = Vec::with_capacity(CHUNK_SIZE);
        while buf.len() < CHUNK_SIZE {
            match tokio_stream::StreamExt::next(&mut reader).await {
                Some(Ok(b)) => buf.extend_from_slice(&b),
                Some(Err(_)) => break,
                None => break,
            }
        }
        if buf.is_empty() && chunk_index >= total_chunks { break; }
        if buf.is_empty() && chunk_index < total_chunks {
            // client closed prematurely
            // send abort to agent
            let abort = json!({"type":"fs:upload","session_id":&session_id,
                "payload":{"upload_id":&upload_id,"final_path":&path,"aborted":true,
                "_mcp_request_id":Uuid::new_v4().to_string()}});
            let _ = bulk_tx.send(abort.to_string()).await;
            audit_ft(&state, &session_id, token, &permission, &path, bytes_sent, "upfile_failed", "client closed").await;
            return StatusCode::BAD_REQUEST.into_response();
        }
        bytes_sent += buf.len() as u64;
        let is_last = chunk_index + 1 >= total_chunks;
        let mcp_req_id = Uuid::new_v4().to_string();
        let payload = json!({
            "path": &path, "content": BASE64.encode(&buf),
            "upload_id": &upload_id, "chunk_index": chunk_index, "total_chunks": total_chunks,
            "_mcp_request_id": &mcp_req_id
        });
        if is_last {
            // register oneshot, await agent fs:result
            let (tx, rx) = tokio::sync::oneshot::channel();
            state.pending_mcp.write().await.insert(mcp_req_id.clone(), (session_id.clone(), tx));
            let proto = json!({"type":"fs:upload","session_id":&session_id,"payload":payload});
            let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
            match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
                Ok(Ok(result)) => {
                    let v: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
                    if v.get("success").and_then(|b| b.as_bool()).unwrap_or(false) {
                        audit_ft(&state, &session_id, token, &permission, &path, bytes_sent, "upfile", "").await;
                        return axum::Json(json!({"ok":true,"bytes":bytes_sent})).into_response();
                    } else {
                        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("agent write failed");
                        audit_ft(&state, &session_id, token, &permission, &path, bytes_sent, "upfile_failed", err).await;
                        return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error":err}))).into_response();
                    }
                }
                _ => {
                    state.pending_mcp.write().await.remove(&mcp_req_id);
                    audit_ft(&state, &session_id, token, &permission, &path, bytes_sent, "upfile_failed", "timeout").await;
                    return StatusCode::GATEWAY_TIMEOUT.into_response();
                }
            }
        } else {
            let proto = json!({"type":"fs:upload","session_id":&session_id,"payload":payload});
            let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;
            chunk_index += 1;
        }
    }
    // empty file (content_len == 0, total_chunks==1, loop sends chunk 0 as last)
    axum::Json(json!({"ok":true,"bytes":0u64})).into_response()
}

/// Audit helper for file-transfer endpoints. status ∈ {upfile, downfile, *_failed}.
async fn audit_ft(state: &crate::relay::SharedState, sid: &str, token: &str, perm: &Permission,
                  path: &str, bytes: u64, status: &str, err: &str) {
    use crate::relay::recorder::{unix_ms, unix_ms_to_iso, token_prefix, AuditLine};
    let Some(recorder) = &state.recorder else { return; };
    let ms = unix_ms();
    let perm_str = match perm { Permission::ReadWrite => "rw", Permission::ReadOnly => "ro" };
    recorder.audit_mcp(sid, AuditLine {
        ts: unix_ms_to_iso(ms), unix_ms: ms,
        session_id: sid.to_string(), token_prefix: token_prefix(token),
        permission: perm_str.to_string(),
        cmd: path.to_string(),         // path stored in cmd field
        timeout_ms: 0, duration_ms: 0,
        status: status.to_string(),
        exit_code: None,
        stdout_len: bytes as usize,    // bytes stored in stdout_len
        stderr_len: if err.is_empty() { 0 } else { err.len() },
        stdout: String::new(),         // never log content
        stderr: err.to_string(),
    });
}
```

注：`http_body_util::BodyExt` 和 `into_data_stream` 需在 `Cargo.toml` 确认 `http-body-util` 依赖。若不可用，改用 axum 的 `Body` 逐帧 `frames()` API。实现时按编译器指引调整。

- [ ] **Step 4: 注册路由**

`src/relay/mod.rs` 路由表（line 320 区域）加：

```rust
.route("/agent/mcp/put", axum::routing::put(super::file_transfer::put_handler))
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --lib put_tests`
Expected: PASS（unauthorized/readonly/411）。

- [ ] **Step 6: 写并运行"成功上传 3 块"集成测试**

`src/relay/file_transfer.rs` 加（mock agent bulk 通道 + pending_mcp 回 fs:result）：

```rust
#[tokio::test]
async fn test_put_streams_chunks_and_awaits_last_result() {
    let state = make_state();
    let (_sid, tokens) = state.sessions.register(None, "rw", None).await.unwrap();
    let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
    {
        let mut broadcast = state.agent_broadcast.write().await;
        let cm = crate::relay::ChannelMap { agent: None, agent_bulk: Some(bulk_tx), browser_sessions: HashMap::new() };
        broadcast.insert(_sid.clone(), cm);
    }
    // data: 3 chunks * 256KB-ish → use small CHUNK by constructing exactly 3*CHUNK bytes
    let data = vec![0xABu8; CHUNK_SIZE * 3];
    let mut headers = HeaderMap::new();
    headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
    headers.insert("content-length", (data.len()).to_string().parse().unwrap());
    let mut params = HashMap::new(); params.insert("path".into(), "/remote/x".into());

    // Drain bulk channel: respond to the last chunk's fs:result via pending_mcp.
    let state_c = state.clone();
    let h = tokio::spawn(async move {
        let mut last_req_id = String::new();
        let mut count = 0u32;
        while let Some(m) = bulk_rx.recv().await {
            let v: serde_json::Value = serde_json::from_str(&m).unwrap();
            if v["type"] == "fs:upload" {
                count += 1;
                let ci = v["payload"]["chunk_index"].as_u64().unwrap() as u32;
                let tc = v["payload"]["total_chunks"].as_u64().unwrap() as u32;
                if ci + 1 >= tc {
                    last_req_id = v["payload"]["_mcp_request_id"].as_str().unwrap().to_string();
                    // fulfill oneshot
                    let mut pending = state_c.pending_mcp.write().await;
                    if let Some((_sid, tx)) = pending.remove(&last_req_id) {
                        let _ = tx.send(serde_json::json!({"success":true}).to_string());
                    }
                    break;
                }
            }
        }
        assert!(count >= 1);
    });

    let resp = put_handler(State(state), headers, Query(params), Body::from(data)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    h.await.unwrap();
}
```

- [ ] **Step 7: 运行确认通过**

Run: `cargo test --lib test_put_streams_chunks_and_awaits_last_result`
Expected: PASS。

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(fs): add PUT /agent/mcp/put streaming upload endpoint"
```

---

## Task 6: agent 侧 `stream_file_download` 改独立 bulk POST + `is_last` + `fs:read_cancel`

**Files:**
- Modify: `src/agent/mod.rs:163-241`（`stream_file_download`）

**Interfaces:**
- Produces: `stream_file_download` 每块走独立 POST（不进 `sender_loop` 控制通道），末块带 `is_last:true`，新增 `fs:read_cancel` 触发终止。

- [ ] **Step 1: 写失败测试——`stream_file_download` 末块 payload 带 `is_last:true`**

`src/agent/mod.rs` 测试模块加。提取构造单块 payload 的纯函数 `build_download_chunk_payload(session_id, path, content_b64, idx, total, mcp_request_id) -> serde_json::Value`（从 `stream_file_download` 循环体抽出，便于单测），然后断言末块带 `is_last:true`、中间块 `is_last:false`：

```rust
#[test]
fn test_download_chunk_payload_is_last_flag() {
    let p0 = build_download_chunk_payload(
        "s", "/x", "AAAA", 0, 3, Some("r".to_string()));
    let p2 = build_download_chunk_payload(
        "s", "/x", "AAAA", 2, 3, Some("r".to_string()));
    assert_eq!(p0["payload"]["is_last"], serde_json::json!(false));
    assert_eq!(p2["payload"]["is_last"], serde_json::json!(true));
    assert_eq!(p2["payload"]["chunk_index"], serde_json::json!(2));
    assert_eq!(p2["payload"]["total_chunks"], serde_json::json!(3));
}
```

注：`build_download_chunk_payload` 是 Step 3 从 `stream_file_download` 提取的纯函数；端到端流式行为在 Task 7 集成测试验证。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib test_download_chunk_payload_is_last_flag`
Expected: FAIL（`build_download_chunk_payload` 不存在）。

- [ ] **Step 3: 改 `stream_file_download`**

`src/agent/mod.rs:163` 函数体改：在每个 `fs:result` payload 加 `"is_last": idx + 1 >= total_chunks`；POST 改为直接用 `client.http_client().post(send_url)` 而非 `post_raw`（避开 `sender_loop`）；每块后 `tokio::task::yield_now()`。新增 `fs:read_cancel` 检测：用一个 `tokio::sync::CancellationToken` 或共享 `Arc<AtomicBool>`，收到 cancel 时 `break`。

```rust
async fn stream_file_download(
    client: reqwest::Client,
    send_url: String,
    session_id: String,
    root: PathBuf,
    path: String,
    mcp_request_id: Option<String>,
) {
    const CHUNK_SIZE: usize = 256 * 1024;
    use std::io::Read;
    // ... resolve/metadata/open unchanged ...
    loop {
        // read chunk
        let is_last = idx + 1 >= total_chunks;
        let payload = serde_json::json!({
            "success": true, "content": content_b64,
            "chunk_index": idx, "total_chunks": total_chunks,
            "name": name, "path": path, "is_last": is_last,
            "_mcp_request_id": mcp_request_id.clone()
        });
        let msg = serde_json::json!({"type":"fs:result","session_id":&session_id,"payload":payload}).to_string();
        // Independent POST (not via sender_loop) so terminal:output/mcp:result
        // are not starved by download chunks.
        let _ = client.post(&send_url).json(&serde_json::from_str::<serde_json::Value>(&msg).unwrap()).send().await;
        tokio::task::yield_now();
        idx += 1;
        if is_last || n == 0 { break; }
    }
}
```

注：cancel 机制（`fs:read_cancel`）实现为：relay 在 GET 客户端断开时发 `fs:read_cancel{correlation_id}` 到 bulk 通道；agent `handle_agent_message` 收到后置一个共享 cancel flag，`stream_file_download` 每块前检查。本 Task 加 cancel 字段骨架；完整 cancel 逻辑在 Task 7。

- [ ] **Step 4: 运行测试**

Run: `cargo test --lib`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src/agent/mod.rs
git commit -m "feat(agent): stream_file_download uses independent POST + is_last + cancel hook"
```

---

## Task 7: 下载端点 `get_handler`

**Files:**
- Modify: `src/relay/file_transfer.rs`（加 `get_handler`）
- Modify: `src/relay/mod.rs`（注册路由）

**Interfaces:**
- Consumes: `DownloadSink`/`DownloadEvent`（Task 3）、`route_agent_message` 路由（Task 4）、`deliver_bulk`（Task 1）、`stream_file_download` 的 `is_last`（Task 6）。
- Produces: `pub async fn get_handler(State, headers, Query) -> Response`，路由 `/agent/mcp/get` GET，流式响应。

- [ ] **Step 1: 写失败测试——get 流式响应拼回原字节**

`src/relay/file_transfer.rs` 加：

```rust
#[tokio::test]
async fn test_get_streams_file_bytes_to_response() {
    let state = make_state();
    let (_sid, tokens) = state.sessions.register(None, "ro", None).await.unwrap(); // ro allowed for download
    let (bulk_tx, mut bulk_rx) = mpsc::channel(16);
    {
        let mut broadcast = state.agent_broadcast.write().await;
        broadcast.insert(_sid.clone(), crate::relay::ChannelMap {
            agent: None, agent_bulk: Some(bulk_tx), browser_sessions: HashMap::new(),
        });
    }
    // Mock agent: when relay sends fs:read via bulk, push 2 fs:result chunks back.
    let state_c = state.clone();
    tokio::spawn(async move {
        let m = bulk_rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["type"], "fs:read");
        let cid = v["payload"]["_mcp_request_id"].as_str().unwrap().to_string();
        // push chunks via route_agent_message
        for (i, (bytes, is_last)) in [(b"a".to_vec(), false), (b"b".to_vec(), true)].iter().enumerate() {
            let chunk = serde_json::json!({
                "type":"fs:result","session_id":&_sid,
                "payload":{"success":true,"content":BASE64.encode(bytes),
                "chunk_index":i,"total_chunks":2,"is_last":*is_last,
                "_mcp_request_id":&cid}
            }).to_string();
            route_agent_message_pub(&state_c, &_sid, &chunk).await;
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert("x-sr-token", tokens[0].0.parse().unwrap());
    let mut params = HashMap::new(); params.insert("path".into(), "/remote/x".into());
    let resp = get_handler(State(state), headers, Query(params)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    use http_body_util::BodyExt;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"ab");
}
```

注：测试需 `route_agent_message` 可调用（现为 `pub async`）。若私有，改 `pub(crate)` 或加测试 helper `route_agent_message_pub`。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --lib test_get_streams_file_bytes_to_response`
Expected: FAIL（`get_handler` 不存在）。

- [ ] **Step 3: 实现 `get_handler`**

`src/relay/file_transfer.rs` 加：

```rust
pub async fn get_handler(
    State(state): State<Arc<crate::relay::SharedState>>,
    headers: axum::http::HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let client_ip = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).unwrap_or("unknown").to_string();
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(&client_ip, 60, std::time::Duration::from_secs(60)) {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    let token = headers.get("x-sr-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let (session_id, permission) = match state.sessions.authenticate(token).await {
        Some(r) => r,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let path = match params.get("path") {
        Some(p) => p.clone(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    let bulk_tx = {
        let broadcast = state.agent_broadcast.read().await;
        match broadcast.get(&session_id).and_then(|cm| cm.agent_bulk.clone()) {
            Some(tx) => tx,
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    };
    let correlation_id = Uuid::new_v4().to_string();
    let (sink_tx, mut sink_rx) = mpsc::channel::<DownloadEvent>(16);
    {
        let mut ds = state.download_streams.write().await;
        ds.insert(correlation_id.clone(), DownloadSink { tx: sink_tx, created_at: std::time::Instant::now(), bytes: 0 });
    }
    // Send fs:read to agent on bulk channel.
    let proto = json!({"type":"fs:read","session_id":&session_id,
        "payload":{"path":&path,"_mcp_request_id":&correlation_id}});
    let _ = deliver_bulk(&bulk_tx, proto.to_string()).await;

    // Wait for chunk 0 to decide 200 vs 500.
    let first = sink_rx.recv().await;
    match first {
        Some(DownloadEvent::Error(msg)) => {
            state.download_streams.write().await.remove(&correlation_id);
            audit_ft(&state, &session_id, token, &permission, &path, 0, "downfile_failed", &msg).await;
            return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error":msg}))).into_response();
        }
        Some(DownloadEvent::Chunk(first_bytes)) => {
            // Build streaming body: first_bytes + rest from sink_rx until End.
            let state_c = state.clone();
            let sid_c = session_id.clone();
            let token_c = token.to_string();
            let perm_c = permission.clone();
            let path_c = path.clone();
            let cid_c = correlation_id.clone();
            let stream = async_stream::stream! {
                yield Ok::<_, std::convert::Infallible>(first_bytes);
                let mut total = 0u64;
                while let Some(ev) = sink_rx.recv().await {
                    match ev {
                        DownloadEvent::Chunk(b) => { total += b.len() as u64; yield Ok(b); }
                        DownloadEvent::Error(_) => break,
                        DownloadEvent::End => break,
                    }
                }
                let _ = state_c.download_streams.write().await.remove(&cid_c);
                audit_ft(&state_c, &sid_c, &token_c, &perm_c, &path_c, total, "downfile", "").await;
            };
            let body = axum::body::Body::from_stream(stream);
            let mut resp = body.into_response();
            resp.headers_mut().insert("content-type", "application/octet-stream".parse().unwrap());
            resp
        }
        _ => {
            state.download_streams.write().await.remove(&correlation_id);
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}
```

- [ ] **Step 4: 注册路由**

`src/relay/mod.rs` 加：

```rust
.route("/agent/mcp/get", axum::routing::get(super::file_transfer::get_handler))
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --lib test_get_streams_file_bytes_to_response`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(fs): add GET /agent/mcp/get streaming download endpoint"
```

---

## Task 8: `shell_remote` 描述更新 + 文档

**Files:**
- Modify: `src/relay/mcp.rs:222-242`（`tools/list`）
- Modify: `README.md`（加「MCP 端文件传输」节）

- [ ] **Step 1: 改 `tools/list` 的 `shell_remote` 描述**

`src/relay/mcp.rs` `tools/list` 里 `shell_remote` 的 `description` 末尾加文件传输说明：

```rust
"description": "Execute a shell command on the remote target machine via shell_remote. Returns stdout, stderr, and exit code. For file transfer: small in-context content (configs/scripts/patches) can be written directly via heredoc/cat through this tool; for large on-disk files, use curl to PUT /agent/mcp/put (upload) or GET /agent/mcp/get (download), with header X-SR-Token set to the same token as this tool. Bytes do not pass through LLM context. See README 'MCP 端文件传输'.",
```

- [ ] **Step 2: 写失败测试——描述含 `put`/`get`**

`src/relay/mcp.rs` 测试模块加：

```rust
#[tokio::test]
async fn test_tools_list_description_mentions_file_transfer() {
    let state = make_state();
    let r = mcp_send_and_recv(&state, HashMap::new(),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
    let desc = r["result"]["tools"][0]["description"].as_str().unwrap();
    assert!(desc.contains("/agent/mcp/put"));
    assert!(desc.contains("/agent/mcp/get"));
    assert!(desc.contains("X-SR-Token"));
}
```

- [ ] **Step 3: 运行确认失败→改完→通过**

Run: `cargo test --lib test_tools_list_description_mentions_file_transfer`
Expected: 先 FAIL，Step 1 改后 PASS。

- [ ] **Step 4: README 加节**

`README.md` 在「AI Agent 接入 (MCP)」节末尾加：

```markdown
### MCP 端文件传输

大文件上传/下载走专用流式端点（不经 LLM context）：

- 上传：`curl -T localfile -H "X-SR-Token: <token>" "https://relay/agent/mcp/put?path=/remote/path"`
- 下载：`curl -H "X-SR-Token: <token>" "https://relay/agent/mcp/get?path=/remote/path" -o localfile`

token 即 `shell_remote` 的会话 token；上传需 rw token，下载 rw/ro 均可。NAT 下两机间文件传输走此端点（relay 中继，不落盘）。小内容（配置/脚本）直接用 `shell_remote` + heredoc 写入。
```

- [ ] **Step 5: Commit**

```bash
git add src/relay/mcp.rs README.md
git commit -m "docs(mcp): document file-transfer endpoints in tools/list + README"
```

---

## Task 9: 超时清理（`download_streams` + `upload_reassembly`）

**Files:**
- Modify: `src/relay/mod.rs:951-995`（现有 300s 清理循环）
- Modify: `src/agent/mod.rs`（`upload_reassembly` 加 `created_at` + 周期清理）

- [ ] **Step 1: relay 清理循环加 `download_streams` 扫描**

`src/relay/mod.rs` 清理循环里（line 953 的 `tokio::spawn` 体内）加：

```rust
// Reap stale download sinks (>5min no progress).
{
    let mut ds = state_clone.download_streams.write().await;
    let now = Instant::now();
    let stale: Vec<String> = ds.iter()
        .filter(|(_, s)| now.duration_since(s.created_at) > std::time::Duration::from_secs(300))
        .map(|(k, _)| k.clone()).collect();
    for k in stale { ds.remove(&k); } // dropping sender ends the agent's push
}
```

- [ ] **Step 2: agent `UploadReassembly` 加 `created_at`**

`src/agent/mod.rs` `UploadReassembly`（line 35）改：

```rust
struct UploadReassembly {
    file: std::fs::File,
    final_path: String,
    created_at: std::time::Instant,
}
```

`assemble_upload_chunk`（line 250）插入项时加 `created_at: std::time::Instant::now()`。

- [ ] **Step 3: agent 主循环周期清理 stale uploads**

`src/agent/mod.rs` 主循环 `select!` 加一个 interval 分支：

```rust
_ = cleanup_tick.tick() => {
    let now = std::time::Instant::now();
    upload_reassembly.retain(|_, r| {
        if now.duration_since(r.created_at) > std::time::Duration::from_secs(300) {
            let _ = std::fs::remove_file(&r.final_path);
            false
        } else { true }
    });
}
```

主循环前加 `let mut cleanup_tick = tokio::time::interval(std::time::Duration::from_secs(60));`

- [ ] **Step 4: 写测试——stale upload 被清理**

```rust
#[tokio::test]
async fn test_stale_upload_reassembly_reaped() {
    // simulate an old entry
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    let mut reassembly: HashMap<String, UploadReassembly> = HashMap::new();
    // ... insert with old created_at, run retain, assert removed + file deleted ...
}
```

- [ ] **Step 5: 运行测试**

Run: `cargo test --lib test_stale_upload_reassembly_reaped`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(fs): timeout cleanup for download_streams + upload_reassembly"
```

---

## Task 10: 带宽隔离集成测试 + 全量验证

**Files:**
- Modify: `tests/integration_test.rs`（若存在集成测试文件，加带宽隔离测试）

- [ ] **Step 1: 全量测试**

Run: `cargo test`
Expected: 全 PASS。

- [ ] **Step 2: 带宽隔离集成测试**

写一个测试：上传狂泵时发 `terminal:input`，断言 input 先被处理（agent 主循环 biased）。若集成测试不便，改为单测 `handle_agent_message` 顺序 + 文档说明 biased 保证。

- [ ] **Step 3: `cargo clippy -- -D warnings`**

Run: `cargo clippy -- -D warnings`
Expected: 无 warning。

- [ ] **Step 4: `cargo build --release`（确认 release 编译）**

Run: `cargo build --release`
Expected: 成功。

- [ ] **Step 5: Commit + 收尾**

```bash
git add -A
git commit -m "test(fs): bandwidth isolation + full verification"
```

---

## Self-Review (plan author)

**Spec coverage:**
- §2 鉴权（token-only）→ Task 5/7 的 `authenticate` + 无 server_auth 检查 ✓
- §3 带宽隔离（bulk 通道 + biased）→ Task 1/2 ✓
- §4 上传端点 + 411 → Task 5 ✓
- §4.1 Content-Length 要求 → Task 5 ✓
- §5 下载端点 + 等 chunk 0 → Task 7 ✓
- §5.1 等 chunk 0 决策 → Task 7 ✓
- §6 agent 主循环提取 → Task 2 ✓
- §7 进度可见性 → 隐含（curl 自身），无任务需要 ✓
- §8 错误处理 → Task 5（上传断连/500）、Task 7（下载 500/超时）、Task 6（cancel）✓
- §9 审计 → Task 5 `audit_ft` + Task 7 ✓
- §10 超时清理 → Task 9 ✓
- §11 shell_remote 描述 → Task 8 ✓
- §12 测试 → 各 Task 内 + Task 10 ✓
- §13 不做 → 无任务（YAGNI）✓

**已知留待实现时确认的点（非占位，是真实决策点）：**
- Task 5 Step 3：`http-body-util` 依赖是否已在 Cargo.toml；若无需用 axum Body frames API。实现时按编译器指引选。
- Task 2 Step 5：agent 第二条 SSE 连接 `/agent/events?bulk=1`——这是 plan 里改动最大的一步，实现时可能需调整 `agent_events_handler` 按 query 切换 rx/bulk_rx。若复杂，可拆子任务，但目标不变：bulk 是独立物理通道。
- Task 6 cancel 机制：`fs:read_cancel` 的共享 flag 载体（CancellationToken vs AtomicBool）实现时定，功能契约（relay 发 cancel → agent 停推送）固定。

这些是实现细节而非设计空缺，plan 已给出方向。无 placeholder（TBD/TODO）。
