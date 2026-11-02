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
- [ ] **补夹具。** 新增类型必须在 `src/proto/fixtures/v<当前版本>/` 的对应方向
      目录下留一条样本，否则覆盖率测试（§7.4）会失败。
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
- [ ] **跑金样与契约测试。** `cargo test --lib proto::fixtures` —— 冻结、回放、
      输出契约、覆盖率四组都要绿（§7）。
- [ ] **跑跨版本矩阵。** `tools/compat_matrix.sh`（见
      [`docs/compat-matrix.md`](compat-matrix.md)）在真机上跑「新 agent × 旧
      relay」「旧 agent × 新 relay」两组组合。
- [ ] **确认新字段的可缺省性。** 手工构造一个删掉新字段的响应/请求，确认代码
      走的是「旧版本行为」分支，而不是报错。
- [ ] **确认没动 `desktop_binary` 这类老兼容位。** §6 的字段清单由测试逐条断言，
      删任何一个都会红。
- [ ] **同步本文。** §2 的清单、§1.3 的能力表、§6 的输出契约字段清单必须与代码
      一致（§6 的清单有测试核对文档与常量的一致性）。
- [ ] **如果要提高 `MIN_PEER_PROTOCOL_VERSION`**：确认线上已无更低版本设备，
      并写进 release note。

---

## 6. 输出契约：旧版本对端依赖的字段（不许删）

下面五份清单是**删除红线**。它们只列「旧版本对端会读」的字段：新增字段不受
限制（老 peer 忽略未知字段），但删掉任何一个，老设备就会静默坏掉——没有报错，
只是注册丢了能力、升级下不动、命令回包对不上。

清单在代码里的常量位置：`src/proto/fixtures.rs` 的 `CONTRACT_*`。测试
`test_contract_*` 断言**当前代码真实构造出的消息**包含这些字段；
`test_contract_field_lists_match_protocol_doc` 断言本节与那些常量逐条一致
（文档漏写也会红）。

### 6.1 relay → agent：`agent:registered`

顶层必须包含：`type`、`session_id`、`evicted`、`desktop_binary`、
`protocol_version`、`relay_version`、`relay_capabilities`、`payload`。

`payload` 必须包含：`tokens`（数组，每项含 `token` 与 `permission`）。

要点：

- `desktop_binary` **必须仍为 `true`**。它是 v0 旧 agent 判断「relay 支持
  `desktop:video` 二进制上行」的唯一依据；`relay_capabilities` 只是新增的替代
  通道，老 agent 完全不认识它。
- `payload.tokens` 是旧 agent 拿身份令牌的唯一来源，格式不能变。

### 6.2 relay → agent：`agent:upgrade.payload`

必须包含：`target_version`、`version`、`filename`、`size`、`sha256`、
`relay_url`、`url`、`relay_fallback`、`source`。

- `version` 是 `target_version` 的**别名**，`url` 是 `relay_url` 的**别名**；
  旧 agent 只读别名，新 agent 优先读主名。两者必须同值
  （`UpgradeTask::wire_payload()` 负责插入别名，测试断言同值）。
- 旧 agent 还硬性要求 `url` 存在，所以没有 relay 副本的任务不能发给它
  （`legacy_compatible()` 判据）。

### 6.3 relay → agent：`mcp:exec.payload`

必须包含：`cmd`、`timeout_ms`、`_mcp_request_id`。

`shell` / `cwd` 是后加的可选键，旧 agent 忽略；`_mcp_request_id` 是回包
关联的唯一凭据，删了命令就永远等不到结果。

### 6.4 relay → agent：`fs:read.payload`

必须包含：`path`、`_mcp_request_id`。

`offset` / `limit` 只在 Range 请求时出现，旧 agent 忽略它们 → 返回整文件
（relay 侧再按 `file_size` 收敛）。

### 6.5 agent → relay：`agent:register`

必须包含：`type`、`agent_version`、`capabilities`、`device`、`protocol_version`。

身份字段二选一：新注册带 `key` + `token_type`，缓存重连带 `tokens`。
`session_id` / `lan_addr` 可选。

**回滚重绑变体**（`update_apply.rs::bind_identity`）同样必须带
`protocol_version`，但**故意不带** `capabilities` / `device` / `lan_addr` ——
relay 侧对这三项是「带了才覆盖」，缺省即保留（§2.3），回滚不该抹掉能力。

---

## 7. 金样夹具与冻结规则

已发布的协议行为用**冻结的 JSON 夹具**钉住，`cargo test` 就是兼容性的守门人。

### 7.1 目录与命名

```
src/proto/fixtures/
  FROZEN.sha256              # 冻结清单：<sha256>  <相对路径>
  v0/                        # 协议标记出现之前的线协议（≤ v0.59.11）
    agent_to_relay/*.json
    relay_to_agent/*.json
  v1/                        # 当前线协议（PROTOCOL_VERSION = 1）
    agent_to_relay/*.json
    relay_to_agent/*.json
```

- 一个消息一个文件，内容是一条**完整信封**（`{type, session_id, payload}` 加上
  该版本特有的顶层字段），因此可以直接喂给真实的路由/解析代码。
- 文件名：`<类型里的冒号换成下划线>[__<变体>].json`。例如
  `terminal_output.json`、`agent_register__reconnect_tokens.json`。
- `desktop:video` 的 9 字节头二进制帧存成 `.bin`
  （`desktop_video__bin_frag.bin`）。
- 必录的变体：注册请求的三种形态（新注册 / 带 tokens 重连 / 回滚重绑）、
  注册响应的 v0 与 v1 两代格式、无 payload 的 `ping`、`agent:upgrade` 的旧别名
  字段格式。

### 7.2 夹具怎么来的

**从真实代码路径产出，不手写。** 生成器 `src/proto/fixtures_gen.rs` 调用生产
构造函数（`agent::client::build_register_message`、
`relay::ws::build_register_response`、`relay::admin::build_upgrade_message`、
`relay::mcp::build_mcp_exec_message`、
`relay::file_transfer::build_fs_read_message`），其余类型复刻各模块
`json!({...})` 里逐字相同的字段集，并在注释里标出源位置。

这些构造函数就是为了让测试断言「真实构造路径」而抽出来的（见 §8 的清单）。

重新生成 `v1/` 并重写冻结清单：

```bash
REGEN_FIXTURES=1 cargo test --lib proto::fixtures::tests::regen_fixtures -- --ignored --nocapture
```

`v0/` **永远不重新生成**——那是已发布的线协议，改动等于伪造历史。

### 7.3 冻结规则

红线：**已发布版本的夹具不允许修改**，也不允许删除。

- `FROZEN.sha256` 记下每个夹具的 sha256，`test_frozen_fixtures_are_unmodified`
  逐条校验。内容一变就失败，提示信息写明：

  > 已发布协议版本的夹具不可修改：要变更协议，请新增 `fixtures/v<N+1>/`，
  > 并保持旧版本夹具继续通过。

- 每个夹具都必须在清单里（`test_every_fixture_is_frozen`），清单也不能有指向
  已删文件的僵尸条目（`test_frozen_manifest_has_no_stale_entries`）。
- 当前开发中、还没发布的最高版本（现在 `v1`）**也一起冻结**。理由：它是
  「当前必须始终通过的基线」，允许随手改就等于允许无痕改协议。真要改线协议，
  走下面的升级流程。

### 7.4 四组测试

跑法：`cargo test --lib proto::fixtures`

| 组 | 测什么 | 关键用例 |
| --- | --- | --- |
| **冻结** | 夹具逐字节未变、清单完整、无僵尸条目 | `test_frozen_*`、`test_every_fixture_is_frozen` |
| **回放（relay 侧）** | 所有版本 `agent_to_relay` 夹具送进 `/agent/send`：注册类得到 `agent:registered`、会话记下正确的协议版本（v0 → 0）；普通消息全被接受、不被判成未知类型；需要广播的类型确实转发出去了 | `test_replay_agent_to_relay_*`、`test_v0_register_without_protocol_version_is_accepted_as_zero`、`test_replay_broadcast_types_reach_browser` |
| **回放（agent 侧）** | 所有版本 `relay_to_agent` 夹具喂给 agent 的信封解析与分发判据：不 panic；v0 注册响应 → relay 协议版本 0、按 `desktop_binary` 回退；`agent:upgrade` 两种格式解析成同一个任务；未知类型/未知字段被忽略 | `test_replay_relay_to_agent_*`、`test_replay_v0_register_response_falls_back_to_desktop_binary`、`test_replay_agent_upgrade_two_formats_yield_same_task` |
| **输出契约** | §6 的字段清单：当前代码发出的五条关键消息都含旧对端要求的全部字段 | `test_contract_*` |
| **覆盖率** | 注册表每个类型在当前版本夹具目录下至少一条（按信封 `type` 判定，不按文件名） | `test_fixture_covers_every_*` |

### 7.5 版本升级时的冻结流程

只有**线协议出现不兼容变化**或**需要一次性声明一批新语义**时才升协议版本号
（§1.2）。走法：

1. 把 `src/proto/mod.rs` 的 `PROTOCOL_VERSION` 加一（如 `1` → `2`）。
2. 新建 `src/proto/fixtures/v2/`，把 `v1/` 的内容拷过去作为起点。
3. 在生成器里按新协议改 `v2` 的夹具（新增字段/类型/变体），
   在 `src/proto/fixtures.rs` 的 `ALL_VERSIONS` 加上 `"v2"`、
   把 `CURRENT_VERSION_DIR` 改成 `"v2"`。
4. 重新生成并重写冻结清单（`regen_fixtures`，见 §7.2）。
5. **`v0/` 与 `v1/` 一个字节都不许动**——它们的 sha256 已经在
   `FROZEN.sha256` 里，改了测试立刻红。
6. 确认 `cargo test --lib proto::fixtures` 全绿：这意味着当前代码能处理
   v0、v1、v2 三代的全部夹具，即「新代码仍然兼容所有历史版本」。
7. 同步本文 §1、§2、§6 与 `docs/compat-matrix.md` 的版本表，写 release note。

> 只做加法的改动（新可选字段、新消息类型、新能力字符串）**不升**版本号，
> 但也必须补夹具——覆盖率测试会拦住「新增类型但没留样本」。

### 7.6 相关文档

- [`docs/compat-matrix.md`](compat-matrix.md)：跨版本兼容矩阵（真实旧二进制
  双向实测）。夹具是「消息级」的单元回归，矩阵是「进程级」的端到端验证，
  两者互补。

---

## 8. 为测试暴露的内部函数

兼容 2 任务为了让夹具和契约测试断言**真实构造路径**（而不是在测试里另抄一份
JSON），把四处内联的 `json!({...})` 抽成了函数。行为零变化，只是把「同一段
构造代码」变成可调用的：

| 函数 | 位置 | 原来在哪 |
| --- | --- | --- |
| `build_register_message` | `src/agent/client.rs` | `RelayClient::connect_http` 里的内联构造 |
| `build_register_response` | `src/relay/ws.rs` | `agent_send_handler` 注册分支的 `Json(json!({...}))` |
| `build_upgrade_message` | `src/relay/admin.rs` | `agent_upgrade_handler` 里的内联构造 |
| `build_mcp_exec_message` | `src/relay/mcp.rs` | `messages_handler` 的 `mcp:exec` 分支 |
| `build_fs_read_message` | `src/relay/file_transfer.rs` | `get_handler` 里的内联构造 |

`build_register_message` 是 `pub(crate)`（agent 内部）；其余四个是 `pub`，
因为夹具生成器与契约测试在 `proto::fixtures` 里跨模块调用它们，而 relay/agent
两个模块之间也需要 `pub` 的实现内可见性。它们都只做「构造一个 JSON 值」，
没有副作用，不暴露任何内部状态。
