# MCP 端文件传输（relay 字节中继端点）

- 日期：2026-07-13
- 状态：设计已确认，待写实现计划
- 范围：relay 新增两个 HTTP 端点，让 LLM 通过本地 Bash `curl` 在 LLM 所在机器与 agent 所在远端机器之间传输任意大小文件，字节不经 LLM context、relay 不落盘、不抢占共享连接带宽。

## 1. 背景与定位

两台目标机器常同处大 NAT 之后，无法接受入站，`rsync`/`scp`/`curl` 端到端均不可用。relay 是两机之间唯一的桥——agent 主动 outbound 连 relay，LLM 端也 outbound 连 relay。因此任何在"LLM 本地文件"与"agent 远端文件系统"之间移动的字节都必须经过 relay。

现有 MCP 表面只有 `shell_remote`（执行命令）。它已覆盖：

- **LLM in-context 内容写远端**：`shell_remote { cmd: "cat > /path << 'EOF'\n<内容>\nEOF" }`，无 base64 膨胀，优于任何 base64 文件工具。
- **小文件读进 LLM context**：`shell_remote { cmd: "cat /path" }`，内容回 stdout，与 base64 工具同量 context 开销但无新工具。

因此本设计**不引入 MCP base64 文件工具**——其场景已被 `shell_remote` 覆盖，且大文件下 base64 会撑爆 LLM context（1MB 文件 ≈ 33 万 token）。

本设计新增两个 relay HTTP 端点，字节以原始流过 HTTP body，relay 做有界缓冲桥接到 agent 现有分块机制，**不落盘、不占 LLM context**。适用 NAT 下任意大小磁盘文件传输。`shell_remote` 的 `tools/list` 描述里加一段使用说明，明确文件传输走 curl 端点。

## 2. 传输拓扑与鉴权

**拓扑**：LLM 是双向发起方。
- 上传：LLM 持会话 token，PUT 本地文件 → relay → agent 远端落盘。
- 下载：LLM 持会话 token，GET → relay → agent 远端读 → LLM 本地落盘。

**鉴权**：两个端点都**只认会话 token，不认服务器密码**。
- `X-SR-Token` header → `state.sessions.authenticate` → `(session_id, permission)`。无效 → 401。
- token 在 header 不在 URL（避免进日志）。这与 `shell_remote`（token 在 arguments）不同，因为本端点不是 MCP tool、无 args，是必要差异。
- 上传：`permission == ReadOnly` → 403。下载：rw/ro 均可（下载是读）。
- UUID（`upload_id` / 下载 `correlation_id`）是**内部 correlation id，不做鉴权**。

## 3. 带宽隔离（核心约束）

约束：文件传输不得饿死共享连接上的交互流量（`terminal:input`、`shell_remote` 派发、其他 fs 操作、其他会话）。

机制：**独立 bulk 子通道 + biased 优先排空**。

- `ChannelMap` 增字段 `agent_bulk: Option<mpsc::Sender<String>>`，容量 **16**（远小于交互通道的 256）。relay→agent 方向的文件分块请求消息（`fs:upload` 分块、`fs:read` 下载请求）走 bulk 通道，其余仍走原 256 交互通道。（bulk 通道是 relay→agent 单向；agent→relay 的下载 `fs:result` 不走它，见下条。）
- agent 主循环 `select!` 用 `biased`：**先 drain 完交互通道所有可用消息，再 drain bulk 通道**。交互流量永远插队到文件块前面。
- bulk 通道背压：文件块**不可丢**，用 `send().await`（不是 `try_send`）。背压只限本传输——交互通道独立，不被背压波及。
- 下载方向（agent→relay）：`stream_file_download` 现直接 `post_raw` 每块进 `sender_loop` 控制通道——改成走**独立 bulk POST**（不进 `sender_loop`，直接用 agent 的 `http_client` 单独发，每块后 `task::yield_now()`）。这样下载 `fs:result` 块不和 `terminal:output`/`mcp:result` 抢 `sender_loop`。
- bulk 通道满时文件块 `send().await` 等待，背压传回 curl（上传）或暂停 agent 推送（下载），交互通道不受影响。

## 4. 上传端点 `PUT /agent/mcp/put`

```
PUT /agent/mcp/put?path=<远端目标路径>
Headers: X-SR-Token: <会话 token>
Body:    文件原始字节流
```

relay 处理（`tokio::spawn`，流式）：
1. 鉴权：`X-SR-Token` → `authenticate` → `(sid, perm)`。无效 → 401。`perm == ReadOnly` → 403。
2. 取 `path` 查询参数；缺失 → 400。
3. 取 `Content-Length`：**缺失 → 411 Length Required**（见 §4.1）。有 → `total_chunks = ceil(len / 256KB)`。
4. `upload_id = uuid()`。
5. 流式读 request body：每次 256KB → base64 → 拼成 `fs:upload` 消息 `{path, content_b64, upload_id, chunk_index, total_chunks, _mcp_request_id}` → `deliver_bulk(agent_bulk_tx, ...)`（`send().await` 背压）。
6. **中间块**（`chunk_index+1 < total_chunks`）：agent 不回 `fs:result`（现有行为）。relay 不等响应，继续读下一块。
7. **最后一块**（`chunk_index+1 == total_chunks`）：relay 注册 `pending_mcp` oneshot（`_mcp_request_id`），发完后 `await rx` 等 agent 的 `fs:result`。
8. `fs:result.success == true` → `200 {"ok":true,"bytes":<字节数>}`；否则 `500 {"error":<agent 错误>}`。
9. 上传途中 curl 断开（body 提前结束、未到 `total_chunks`）→ relay 发"上传中止"信号给 agent（见 §8），清理 `pending_mcp`，响应 `400 {"error":"client closed prematurely"}`。

agent 侧：**零改动**。现有 `fs:upload` arm + `assemble_upload_chunk` 原样处理，最后一块回 `fs:result`。

### 4.1 为何要求 Content-Length

agent 现有 `assemble_upload_chunk` 要 `total_chunks` 判最后一块（`is_last = total_chunks > 0 && chunk_index + 1 >= total_chunks`）。要求 curl 带 `Content-Length` 让 relay 提前算出 `total_chunks`，agent 逻辑零改。`curl -T 普通文件` 自带 CL。流式生成、不知大小的场景不适用本端点——那种场景用 `shell_remote`+heredoc 更合适。

## 5. 下载端点 `GET /agent/mcp/get`

```
GET /agent/mcp/get?path=<远端源路径>
Headers: X-SR-Token: <会话 token>
Response: 文件原始字节流 (chunked transfer-encoding)
```

relay 处理（流式）：
1. 鉴权同上传（ro 允许）。`path` 缺失 → 400。
2. `correlation_id = uuid()`。
3. 注册 **streaming sink**：`state.download_streams`（新表 `RwLock<HashMap<String, DownloadSink>>`，容量 16），`correlation_id → sink`。`DownloadSink` 持一个 `mpsc::Sender<DownloadEvent>`（容量 16）+ 创建时间戳 + 字节数累计。`DownloadEvent` 枚举：`Chunk(Vec<u8>)`（一块原始字节）、`Error(String)`（agent 报错）、`End`（末块后）。这是 relay 把 agent 推回的 `fs:result` 块路由到本请求响应流的内部桥。
4. 向 agent 发 `fs:read` 消息 `{path, _mcp_request_id: correlation_id}`，**走 bulk 通道**。
5. **等 chunk 0**：relay 不立即回 200。agent 现有 `stream_file_download` 推送 N 个 `fs:result`（每个带 `content_b64` + `chunk_index` + `total_chunks` + `correlation_id`，chunk 0 还带 `size`）。relay 的 `route_agent_message` 检测 `fs:result` 带 `correlation_id` 且在 `download_streams` 表里 → 解析：`success:false` → `sink.send(Error(msg))`；否则解 base64 → `sink.send(Chunk(bytes))`；末块 → `sink.send(End)`。全部 `send().await` 背压。
6. **chunk 0 是错误**（首个 `DownloadEvent` 是 `Error`）→ relay 回 `500 {"error":<msg>}`，移除 sink。
7. **chunk 0 正常**（首个 `DownloadEvent` 是 `Chunk`）→ relay 回响应头 `200`、`Content-Type: application/octet-stream`、`Transfer-Encoding: chunked`、`X-SR-Size: <字节数>`、`X-SR-Total-Chunks: <n>`，把首块字节写进响应 body，然后继续从 sink `recv` 写 body，直到 `End`。流中收到 `Error` → 写 `X-SR-Error` 响应头（trailer 不可用则写进 body 尾部）后关 body。
8. curl 用 `-o localfile` 收，字节落盘，不经 LLM context。

agent 侧：**几乎零改**。`stream_file_download` 已是推送式分块，relay 加一个 sink 路由分支把 `fs:result` 块解 base64 转发进响应 body。

### 5.1 为何等 chunk 0 再回响应

首块是错误时能回 500（而非已发 200 后只能把错误塞 body、curl 退出码骗人）。代价：正常文件多一个首块（≤256KB）的延迟才开流。已确认可接受。

## 6. agent 主循环改动（带宽复用点）

现有 `fs:upload`/`fs:read` 处理内联在主循环 match。bulk 通道接入方式：

- 提取 `handle_agent_message(msg, ...)` 函数，封装现有消息分发逻辑。
- 主循环 `select!`（`biased`）三分支：交互通道 recv → `handle_agent_message`；bulk 通道 recv → `handle_agent_message`；其他（shell output、定时器）。
- `biased` 顺序：交互通道优先，bulk 其次。交互先排空，bulk 再处理。

`upload_reassembly` 表与 `download_handles`（§10）仍由 `handle_agent_message` 持有/更新。

## 7. 进度可见性

curl 跑在 LLM Bash 里，LLM 启动后脱手。进度靠 curl 自身（`--progress-bar` 输出到 stderr），LLM 可在 Bash 里看到。relay 不额外提供进度查询端点（YAGNI）。传输完成 curl 退出码=0 即成功。LLM 若要确认远端文件，用 `shell_remote { cmd: "ls -l /path" }`。

## 8. 错误处理

| 场景 | 行为 |
|------|------|
| 鉴权失败（token 无效） | 401，不触达 agent |
| 权限不足（ro 上传） | 403 |
| `path` 缺失 | 400 |
| 上传缺 Content-Length | 411 |
| agent 未连（`agent_broadcast` 无该 sid） | 503 `{"error":"No agent connected"}` |
| agent `fs:result.success=false`（resolve 失败、写失败等） | 上传：500 带错误；下载：chunk 0 错误 → 500，流中错误 → 关 body + `X-SR-Error` 响应头 |
| 上传中途 curl 断开 | relay 发"上传中止"给 agent（`fs:upload` 带 `upload_id` + `aborted:true` 标记），agent 删 `upload_reassembly` 项 + 删半成品文件。响应 400。 |
| 下载中途 curl 断开 | relay 检测到响应写失败 → 从 `download_streams` 移除 sink。agent 的 `stream_file_download` 下次 `post_raw` 失败时停止推送（agent 已有 POST 失败日志，加一个"sink 消失则停止"的感知：relay 在移除 sink 时通过 bulk 通道回一个 `fs:read_cancel` 给 agent，agent 收到后终止对应 `stream_file_download` 任务）。 |
| 超时 | §10 清理；curl 侧 `--max-time` 兜底。 |

## 9. 审计

`--record-dir` 开启时，上传/下载各写一条 `AuditLine`：`status` 用 `"upfile"`/`"downfile"`，记 `path` + `bytes` + `permission` + `token_prefix`，**不记内容**。复用 `recorder.audit_mcp` 通道（不阻塞热路径）。失败也记一条（`status` 带 `_failed` 后缀）。与现有 `shell_remote` 审计同开关、同目录、同每会话文件。

## 10. 超时与清理

- `download_streams` 项：注册时记时间戳。复用现有 300s 清理循环，扫 `download_streams`，> 5 分钟未写入字节且未完成的 sink → 关闭 sender + 移除。agent 侧对应推送任务下次 POST 失败自然终止。
- `upload_reassembly`（agent 侧）：现有表无时间戳。加 `created_at` 字段，agent 主循环周期检查（复用现有定时器或加一个 interval），> 5 分钟无新块的 `upload_id` → 删半成品文件 + 移除项。防 curl 断在中间泄漏。
- curl 侧用 `--max-time` / `--speed-limit` 兜底。

## 11. `shell_remote` 描述更新

`tools/list` 里 `shell_remote` 的 `description` 末尾加：

> 对于文件传输：LLM in-context 的配置/脚本/补丁等小内容，直接用本工具 heredoc/cat 写入远端；磁盘上的大文件上传/下载，用 curl 调 `/agent/mcp/put`（上传）与 `/agent/mcp/get`（下载），header 带 `X-SR-Token`（即本工具的 `token`），字节不经 LLM context。详见 README「MCP 端文件传输」。

## 12. 测试

- 上传成功：mock agent，发 3 块文件，验证 agent 收到 3 个有序 `fs:upload` + 最后一块后 relay 回 200 + 远端文件字节正确。
- 下载成功：mock agent 推 3 个 `fs:result`，验证 relay 响应 body 拼回原字节。
- 权限：ro token PUT → 403；ro token GET → 200。
- 鉴权：缺 `X-SR-Token` / 错 token → 401；无服务器密码要求（不传 `X-Auth` 也通过）。
- Content-Length：PUT 无 CL → 411。
- 带宽隔离：上传狂泵时发一条 `terminal:input`，验证 agent 先处理 input 再继续上传块（biased 顺序单测）。
- 上传断连：中途断 body → relay 清理 `upload_reassembly` + 400。
- 下载断连：中途断 → sink 移除 + agent 推送终止。
- 下载 chunk 0 错误：agent 首块 `success:false` → relay 回 500。
- 审计：上传/下载成功/失败各写一条，内容不记，`status` 正确。
- 清理：`download_streams` 5 分钟超时移除；`upload_reassembly` 5 分钟超时删半成品。

## 13. 不做（YAGNI）

- MCP base64 文件工具（`upfile_remote`/`downfile_remote`）：场景被 `shell_remote` 覆盖。
- 进度查询端点：curl 自身进度够用。
- 断点续传：YAGNI；失败重传整个文件即可。
- 多文件/目录递归传输：用 `shell_remote` 跑 `tar` 管道。
- 服务器密码校验：本端点只认 token（设计决策）。
- UUID 做下载鉴权：UUID 是内部 correlation，鉴权统一走 token。
