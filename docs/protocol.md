# agent ↔ relay 线协议与兼容规范

本文是 shell-remote 的 **agent ↔ relay 线协议**（wire protocol）的权威说明，也是
**改协议时的自查依据**。目标只有一个：任何时候都能回答「新 agent 配旧 relay、
旧 agent 配新 relay，会不会坏」。

- 适用范围：agent 与 relay 之间的 HTTP / SSE / WebSocket 线协议。
  浏览器 ↔ relay 的通道只在涉及广播语义时提及。
- 代码位置全部标到 `文件:行`；行号会随代码变动，**以符号名为准**。

---

## 1. 协议版本与能力声明

### 1.1 两个常量

| 常量 | 位置 | 值 | 含义 |
| --- | --- | --- | --- |
| `PROTOCOL_VERSION` | `src/proto/mod.rs:25` | `1` | 本端实现的线协议版本 |
| `MIN_PEER_PROTOCOL_VERSION` | `src/proto/mod.rs:30` | `0` | 仍愿意兼容的最老对端版本 |

版本约定：

- `0` = **协议标记出现之前**的旧版本（≤ v0.59.11）。注册消息里没有
  `protocol_version` 字段。
- `1` = 本版引入协议标记后的线协议：注册握手带 `protocol_version`，relay 的
  `agent:registered` 响应带 `protocol_version` / `relay_version` /
  `relay_capabilities`。

### 1.2 版本号管什么、不管什么

**版本号不是功能开关。** 某个功能是否可用，只看能力字符串。版本号只承担三件事：

1. **粗粒度判定**——快速区分「带协议标记的一侧」与「标记出现前的老侧」；
2. **诊断**——注册日志和管理后台能一眼看出对端是哪一代；
3. **界定支持窗口**——与 `MIN_PEER_PROTOCOL_VERSION` 一起说明还愿意兼容到哪一代。

升版本号的**唯一**理由：线协议出现不兼容变化，或需要一次性声明一批新语义
（例如某类消息的语义整体改变）。**只做加法的兼容改动不升版本号**——新可选
字段、新消息类型、新能力字符串都属于加法，老 peer 忽略未知字段/类型即可。

> 反例（不要这么做）：为了「加一个桌面编码开关」把版本号从 1 升到 2。这会让
> 所有按版本号做判断的代码路径瞬间分叉，而真正需要的只是能力字符串
> `codec:av1`。

### 1.3 能力字符串

能力是**功能级**的声明，方向按「谁声明、谁消费」区分。

agent → relay（注册消息的 `capabilities` 数组，构建于
`src/agent/client.rs:193` `build_capabilities()`）：

| 能力 | 声明条件 |
| --- | --- |
| `web:proxy-v1` | 恒声明（`crate::agent::web_proxy::CAPABILITY`） |
| `net:tcp-v1` | 恒声明（`crate::agent::tcp_tunnel::CAPABILITY`） |
| `upgrade:v2` | 恒声明（`crate::agent::download::CAPABILITY_UPGRADE_V2`） |
| `variant:cli` / `variant:ui` | 制品形态可判定时才声明 |
| `codec:h264` | 恒声明 |
| `codec:av1` | 编译 `av1` feature |
| `backend:x11` | `target_os = "linux"` |
| `backend:wayland` | linux + `wayland` feature + `WAYLAND_DISPLAY` 可达 |
| `backend:gdi` / `backend:dxgi` | `target_os = "windows"` |
| `desktop:gray` / `desktop:quality` / `desktop:clipboard` / `desktop:cursor` / `desktop:test-delay` | 恒声明 |

relay → agent（注册响应的 `relay_capabilities` 数组，常量
`src/proto/mod.rs:39` `RELAY_CAPABILITIES`）：

| 能力 | 含义 |
| --- | --- |
| `desktop:binary-uplink` | agent→relay 的 `desktop:video` 支持二进制 WS 帧上行 |

新增 relay 能力必须同时加进 `RELAY_CAPABILITIES`，否则 agent 永远看不到它。

---

## 2. 线协议清单

### 2.1 端点

| 端点 | 方向 | 用途 | 位置 |
| --- | --- | --- | --- |
| `POST /agent/send` | agent → relay | 注册、控制消息、回包（JSON 单条或批量数组） | `src/relay/mod.rs:2613` |
| `GET /agent/events?session=` | relay → agent | SSE 下行（消息流 + 5s keep-alive 注释） | `src/relay/mod.rs:2615` |
| `GET /agent/ws/send?session=` | agent → relay | 桌面视频 WS 上行（text=JSON，binary=帧格式见 §2.4） | `src/relay/mod.rs:2614` |
| `GET /agent/session/sse?token=` | relay → 浏览器 | 浏览器 SSE 下行 | `src/relay/mod.rs:2608` |
| `POST /agent/session/send` | 浏览器 → relay | 浏览器控制输入 | `src/relay/mod.rs:2610` |
| `GET /agent/desktop/stream` / `/agent/desktop/ws` | relay → 浏览器 | 桌面流（HTTP 分片 / WS） | `src/relay/mod.rs:2645`、`:2649` |
| `POST /agent/web/ticket`、`GET /agent/web/proxy` | 浏览器 → relay | 网页代理（MYS-2383） | `src/relay/mod.rs:2631`、`:2635` |
| `POST /agent/web/map`、`/agent/web/m/:id/*` | 浏览器 → relay | 同源网页映射 v2 | `src/relay/mod.rs:2640`、`:2644` |
| `PUT /agent/mcp/put`、`GET /agent/mcp/get` | 客户端 ↔ relay | 文件传输 | `src/relay/mod.rs:2626`、`:2629` |
| `GET /agent/install`、`/agent/install.ps1` | 客户端 | 一键安装脚本 | `src/relay/mod.rs:2674`、`:2675` |
| `GET /agent/resolve` | agent → relay | DNS 代解析（第三级兜底） | `src/relay/mod.rs:2673` |
| `GET /agent/upgrade/blob/:filename` | agent → relay | 升级制品下载 | `src/relay/mod.rs:2651` |

### 2.2 消息信封

所有 JSON 消息共用同一个信封（`src/proto/mod.rs:176` `Message`）：

```json
{ "type": "terminal:input", "session_id": "abc123", "payload": { } }
```

| 字段 | 必填 | 缺省 | 说明 |
| --- | --- | --- | --- |
| `type` | **是** | — | 消息类型，见 §2.3 注册表 |
| `session_id` | 否 | `""` | 会话标识。心跳 `ping` 不带 |
| `payload` | 否 | `null` | 消息体。缺失时为 `Null`，所有消费方在 `Null` 上按 key 取值都得到 `None`/默认值 |

**唯一例外**：心跳 `{"type":"ping","session_id":...,"kpi":{...}}` 没有 `payload`，
relay 在严格解析**之前**先用宽松 JSON 拦截（`src/relay/ws.rs:429`），采样 KPI 后
直接返回，不转发。

### 2.3 注册握手

**agent → relay**（`src/agent/client.rs:451` 起，回滚重绑
`src/agent/update_apply.rs:2703` 同构）：

```json
{
  "type": "agent:register",
  "key": "<--key，可选>",
  "token_type": "rw | ro | both",
  "session_id": "<自定义 id，可省略>",
  "tokens": [ { "token": "...", "permission": "rw|ro" } ],
  "agent_version": "0.59.11",
  "protocol_version": 1,
  "capabilities": ["web:proxy-v1", "codec:h264", "..."],
  "device": { "hostname": "...", "platform": "...", "arch": "...", "os": "...", "kernel": "...", "cpu_model": "..." },
  "lan_addr": "192.168.1.5:43210"
}
```

字段口径（relay 侧 `src/relay/ws.rs:1276` 起）：

| 字段 | 缺省行为 |
| --- | --- |
| `tokens` | 非空数组 → 走「缓存 token 重连」（`register_existing`）；否则按 `key`/`token_type` 新建 |
| `agent_version` | 缺失 → `None`（老 agent 不报版本） |
| `protocol_version` | 缺失/非法 → `0`（`src/proto/mod.rs:44` `parse_protocol_version`），存进 `SessionInfo::agent_protocol_version`（`src/relay/session.rs:30`），管理后台随 `agent_version` 一起展示（`src/relay/admin.rs:192`、`:1014`、`:1375`） |
| `capabilities` | **字段缺省 = 保持会话上已有的值**；字段存在（含 `[]` / `null` / 非数组）= 覆盖，`[]` 表示「这一代不支持任何能力」 |
| `device` | 同上：缺省保留原值 |
| `lan_addr` | 同上：缺省保留原值 |

> 为什么 capabilities / device / lan_addr 是「带了才覆盖」：注册会把整个会话
> 条目换掉（`register` / `register_existing` 丢旧条目再插新的）。回滚重绑消息
> 只带身份，若按「缺省 = 写空」处理，一台设备的全部能力与硬件信息会在升级回滚
> 后被抹掉（`src/relay/ws.rs` 注册处理里的 `carried` 快照就是为了回填这三项）。

**relay → agent**（`src/relay/ws.rs:1573` 起）：

```json
{
  "type": "agent:registered",
  "session_id": "abc123",
  "evicted": false,
  "desktop_binary": true,
  "protocol_version": 1,
  "relay_version": "0.59.11",
  "relay_capabilities": ["desktop:binary-uplink"],
  "payload": { "tokens": [ { "token": "...", "permission": "rw" } ] }
}
```

agent 侧解析在 `src/agent/client.rs:371` `parse_register_handshake`：

- `protocol_version` 缺失/非法 → `0`（`RelayClient::relay_protocol_version`）；
- `relay_version` 缺失 → `None`；
- `desktop_binary` 的判定顺序（`src/agent/client.rs:301` `parse_desktop_binary`）：
  1. 响应带 `relay_capabilities` 数组 → **只看**里面有没有
     `desktop:binary-uplink`（数组存在即权威声明：没列就是明确不支持，即使
     `desktop_binary: true`）；
  2. 该字段不存在（旧 relay）→ 回退到 `desktop_binary: true`；
  3. 两者皆无 → `false`，agent 保持 JSON+base64。
- `payload.tokens` **逐条宽松解析**（`src/agent/client.rs:320`
  `parse_tokens_lenient`）：单条缺 `token`/`permission` 的条目跳过并记 warn；
  全部条目无效 → 空列表，等同「relay 没给 tokens」，走旧 relay 回退路径
  （上行不带 `X-SR-Agent-Token`）。
- 注册成功后打一条 info 日志：`registered with relay v<X> (protocol <N>)`。

`desktop_binary` **必须保留**：旧 agent 只认它，一个字段都不认识
`relay_capabilities`。

### 2.4 消息类型注册表

唯一的登记处是 `src/proto/mod.rs`：

- `AGENT_TO_RELAY_TYPES`（`src/proto/mod.rs:63`）
- `RELAY_TO_AGENT_TYPES`（`src/proto/mod.rs:109`）
- 判据函数 `is_known_agent_type` / `is_known_relay_type`（`src/proto/mod.rs:166`、`:171`）

方向的判定口径：**消息由谁首次发出**。relay 的「已知类型」判断直接引用
`AGENT_TO_RELAY_TYPES`（`src/relay/ws.rs` 的 `route_agent_message` 尾部），不再
维护第二份手写列表——此前那份列表漏了 `desktop:p2p-answer` /
`desktop:p2p-state`，导致每条 P2P 信令都白打一条 unknown 日志。

**agent → relay**：`ping`、`agent:upgrade_progress`、`terminal:output`、
`fs:result`、`fs:mkdir`、`mcp:result`、`mcp:exec_result`、`session:users`、
`session:tab_list`、`session:tab_switched`、`desktop:video`、`desktop:started`、
`desktop:stopped`、`desktop:error`、`desktop:capabilities`、`desktop:uplink`、
`desktop:clipboard`、`desktop:qos-ack`、`desktop:cmd-ack`、`desktop:cursor`、
`test-delay-ack`、`desktop:p2p-answer`、`desktop:p2p-state`、`web:head`、
`web:chunk`、`web:end`、`web:error`、`tcp:opened`、`tcp:data`、`tcp:ack`、
`tcp:eof`、`tcp:closed`、`tcp:error`、`error`。

**relay → agent**：`terminal:input`、`terminal:resize`、`session:join`、
`session:leave`、`session:tab_switch`、`session:tab_create`、`session:tab_close`、
`fs:list`、`fs:read`、`fs:read_cancel`、`fs:write`、`fs:upload`、`fs:delete`、
`fs:rename`、`fs:mkdir`、`mcp:exec`、`mcp:exec_start`、`mcp:exec_input`、
`mcp:exec_close`、`mcp:exec_list`、`desktop:start`、`desktop:stop`、
`desktop:codec`、`desktop:quality`、`desktop:bitrate`、`desktop:gray`、
`desktop:reqkey`、`desktop:test-delay`、`desktop:mouse`、`desktop:key`、
`desktop:clipboard:get`、`desktop:clipboard:set`、`desktop:select-display`、
`desktop:qos`、`desktop:congested`、`desktop:p2p-offer`、
`desktop:p2p-candidate`、`web:request`、`web:cancel`、`tcp:open`、`tcp:data`、
`tcp:ack`、`tcp:eof`、`tcp:close`、`agent:upgrade`。

真正**双向**的类型（请求与回包/广播共用同一类型名）：`tcp:data`、`tcp:ack`、
`tcp:eof`、`fs:mkdir`。

未知类型的行为两侧一致——**忽略**，不报错：

- agent：`src/agent/mod.rs` 的消息循环 `other => tracing::debug!("Unknown message type: {}", other)`；
- relay：`route_agent_message` 尾部记一条 info（`dropping unknown agent message
  (not in registry)`）后丢弃。

### 2.5 二进制帧格式（`desktop:video` 上行）

走 `GET /agent/ws/send` 的 WS **binary** 帧（格式规格与编解码
`src/proto/mod.rs:452`–`:490`）：

```
[flags:u8][seq:u32 LE][len:u32 LE][payload len 字节]
```

- `flags` 位 0：`1` = key frame（IDR），`0` = delta；
- `flags` 位 1：`1` = init 段（fMP4 ftyp/moov）；
- `len` 显式携带，与 WS 帧边界做**双重校验**：`len` 与实际剩余字节不符 →
  `decode_bin_frame` 返回 `None`，relay 直接丢弃这一帧（不 panic、不吞错）。

是否启用二进制上行由 §2.3 的 `desktop:binary-uplink` 能力决定；未启用时同一
消息走 JSON + base64（payload 里带 `kind` / `data` / `seq` / `key`），行为与旧
版本完全一致。

---

## 3. 兼容规则

改协议时**必须**满足以下全部条款。它们就是「新 agent 配旧 relay、旧 agent 配
新 relay 都能跑」的充分条件。

1. **只做加法。** 只能新增字段、新增消息类型、新增能力字符串。不得删除字段、
   不得改变已有字段的含义或类型、不得复用已废弃的字段名。
2. **新字段必须可选且有默认值。** 线上没有 `deny_unknown_fields`，未知字段两侧
   都会被忽略；但要读的新字段必须能缺席，且缺席时的默认值等于「旧版本行为」。
   - 布尔/整数字段：缺席 → `false` / `0`；
   - 数组字段：缺席 → 空数组；
   - 字符串字段：缺席 → `None` 或 `""`；
   - 复用字段做「存在即覆盖」判定时（如 `capabilities` / `device` /
     `lan_addr`），必须同时给出「缺席 = 保持原值」的语义。
3. **新功能放在能力字符串后面。** 先声明能力，再按能力启用行为。不要让版本号
   决定某个功能开不开——版本号是粗粒度的。
4. **未知类型和未知字段一律忽略。** 不得因为不认识就断开、报错、或让整个
   注册失败。畸形**值**（类型不对的内容）与未知**字段**要区别对待：畸形值按
   默认值处理并记日志，未知字段静默忽略。
5. **单条坏数据不得拖垮整条流程。** 集合里的一个坏元素（如 `tokens` 里缺
   `token` 的条目）应跳过并记 warn，而不是让整个注册失败。
6. **删除旧路径之前，必须先把 `MIN_PEER_PROTOCOL_VERSION` 提高，并写进 release
   note。** 「不再兼容 v0.x」这种决定必须显式、可审计，不能靠代码里悄悄删掉
   一个分支完成。
7. **`desktop_binary` 这类老兼容位不许删。** 只要还有旧 agent 在线上，新 relay
   就必须继续发它。替代字段（`relay_capabilities`）只能**新增**，靠「新字段
   存在就优先看新字段」来演进。

---

## 4. 支持窗口

- **下限**：至少兼容**线上设备里最旧的版本**。当前线上最旧的是 **v0.53.0**
  （2026-11-02 统计：0.59.4×8、0.59.8×4、0.59.3×1、0.53.0×1）。
- **协议版本下限**：`MIN_PEER_PROTOCOL_VERSION = 0`，即仍兼容所有「协议标记出现
  之前」的版本（≤ v0.59.11）。v0.53.0 落在这个窗口内。
- **提高下限的流程**：先确认线上已无该版本设备（管理后台的设备列表按
  `agent_version` 排序即可），再改常量、写 release note，并同步更新本节。

---

## 5. 改动协议时的自查清单

改任何与线协议相关的东西之前，逐条过一遍：

- [ ] **登记消息类型。** 新增的消息类型加进 `AGENT_TO_RELAY_TYPES` /
      `RELAY_TO_AGENT_TYPES`（`src/proto/mod.rs`），否则 relay 会把它当未知类型
      丢弃，本文 §2.4 的清单也会失真。
- [ ] **补新旧双向单测。** 至少两条：一条「新对端 + 新字段」、一条「旧对端 +
      缺字段」。参考现有用例：
      - `src/agent/client.rs` — `test_parse_register_handshake_from_old_relay`、
        `test_parse_register_handshake_from_new_relay`、
        `test_desktop_binary_prefers_relay_capabilities`、
        `test_parse_register_handshake_lenient_tokens`；
      - `src/relay/ws.rs` — `test_register_stores_agent_protocol_version`、
        `test_register_response_declares_protocol_and_capabilities`、
        `test_rebind_without_capabilities_keeps_them`；
      - `src/proto/mod.rs` — `test_message_parses_without_payload_or_session_id`、
        `test_message_type_registry`、`test_parse_protocol_version`。
- [ ] **跑跨版本矩阵。** `tools/compat_matrix.sh`（由并行任务 MYS-2553 1b 提供）
      在真机上跑「新 agent × 旧 relay」「旧 agent × 新 relay」两组组合。
- [ ] **确认新字段的可缺省性。** 手工构造一个删掉新字段的响应/请求，确认代码
      走的是「旧版本行为」分支，而不是报错。
- [ ] **确认没动 `desktop_binary` 这类老兼容位。**
- [ ] **同步本文。** §2 的清单与 §1.3 的能力表必须与代码一致。
- [ ] **如果要提高 `MIN_PEER_PROTOCOL_VERSION`**：确认线上已无更低版本设备，
      并写进 release note。
