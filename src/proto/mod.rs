#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ── 协议版本（MYS-2553 兼容 1a）────────────────────────────────────
//
// 线协议上的**粗粒度**版本号。它不是功能开关：某个功能是否可用仍然只看
// 能力字符串（`web:proxy-v1` / `upgrade:v2` / `desktop:binary-uplink` 等）。
// 版本号只承担三件事：
//   1. 粗粒度判定——快速区分「带协议标记的一侧」与「标记出现前的老侧」；
//   2. 诊断——注册日志/管理后台能一眼看出对端协议代次；
//   3. 界定支持窗口——与 [`MIN_PEER_PROTOCOL_VERSION`] 一起说明还愿意兼容
//      到哪一代。
//
// 升版本号的**唯一**理由：线协议出现不兼容变化，或需要一次性声明一批新语义
// （例如某类消息的语义整体改变）。只做加法的兼容改动（新可选字段、新消息
// 类型、新能力字符串）**不升**版本号——老 peer 忽略未知字段/类型即可。
//
// 版本约定：
//   - `0`：标记出现之前的旧版本（≤ v0.59.11），注册消息里没有
//     `protocol_version` 字段；
//   - `1`：本次引入协议标记后的线协议（注册握手带 `protocol_version`，
//     relay 注册响应带 `protocol_version` / `relay_version` /
//     `relay_capabilities`）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 本端仍愿意与之为伍的**最老**对端协议版本。低于它的对端可以直接拒绝
/// （或在文档里声明「不再保证兼容」）。提高它属于破坏性变更，必须写进
/// release note。当前 `0` = 仍兼容「标记出现之前」的所有旧版本。
pub const MIN_PEER_PROTOCOL_VERSION: u32 = 0;

/// relay 能力字符串：agent→relay 的 desktop:video 支持二进制 WS 帧上行
/// （[`encode_bin_frame`] 格式）。新 relay 在注册响应的 `relay_capabilities`
/// 里声明；老 relay 只能靠 `desktop_binary: true` 这个布尔字段告知。
pub const CAP_DESKTOP_BINARY_UPLINK: &str = "desktop:binary-uplink";

/// 当前 relay 声明的全部能力（注册响应 `relay_capabilities` 的内容）。
/// agent 判定某个 relay 能力时**必须**看这个列表，而不是看 relay 版本号。
pub const RELAY_CAPABILITIES: &[&str] = &[CAP_DESKTOP_BINARY_UPLINK];

/// 解析对端在注册握手/响应里声明的 `protocol_version`：缺失、非整数、
/// 负数一律记为 [`MIN_PEER_PROTOCOL_VERSION`]（= 旧版本），绝不因为一个
/// 可疑的版本字段让注册失败。
pub fn parse_protocol_version(value: Option<&serde_json::Value>) -> u32 {
    match value.and_then(|v| v.as_u64()) {
        Some(n) => n.min(u32::MAX as u64) as u32,
        None => MIN_PEER_PROTOCOL_VERSION,
    }
}

// ── 消息类型注册表（MYS-2553 兼容 1a）──────────────────────────────
//
// 全部线上消息类型的**唯一**登记处。新增消息类型必须同时加进对应方向的
// 常量，否则 relay 会把它当成未知类型丢弃（并在日志里报 unknown），
// `docs/protocol.md` 的清单也会失真。
//
// 方向的判定口径：消息由谁**首次发出**。因此 `desktop:p2p-answer` /
// `desktop:p2p-state` 是 agent→relay（agent 应答/上报状态后由 relay 广播
// 给浏览器），而 `desktop:p2p-offer` / `desktop:p2p-candidate` 是
// relay→agent（浏览器发起，relay 转发）。

/// agent → relay 方向的全部消息类型。
pub const AGENT_TO_RELAY_TYPES: &[&str] = &[
    // 生命周期 / 保活（`ping` 无 payload，路由前按宽松 JSON 单独拦截）
    "ping",
    "agent:upgrade_progress",
    // 终端 / 文件 / MCP 回包
    "terminal:output",
    "fs:result",
    "fs:mkdir",
    "mcp:result",
    "mcp:exec_result",
    // 会话状态
    "session:users",
    "session:tab_list",
    "session:tab_switched",
    // 桌面
    "desktop:video",
    "desktop:started",
    "desktop:stopped",
    "desktop:error",
    "desktop:capabilities",
    "desktop:uplink",
    "desktop:clipboard",
    "desktop:qos-ack",
    "desktop:cmd-ack",
    "desktop:cursor",
    "test-delay-ack",
    // Task 2 P2P 信令（agent 应答 / 状态上报，relay 广播给浏览器）
    "desktop:p2p-answer",
    "desktop:p2p-state",
    // 网页代理回包（MYS-2383）
    "web:head",
    "web:chunk",
    "web:end",
    "web:error",
    // 纯 TCP 管道回包（MYS-2383 阶段6）
    "tcp:opened",
    "tcp:data",
    "tcp:ack",
    "tcp:eof",
    "tcp:closed",
    "tcp:error",
    // 通用错误回包（只读会话拒绝写操作等）
    "error",
];

/// relay → agent 方向的全部消息类型。
pub const RELAY_TO_AGENT_TYPES: &[&str] = &[
    // 终端
    "terminal:input",
    "terminal:resize",
    // 会话
    "session:join",
    "session:leave",
    "session:tab_switch",
    "session:tab_create",
    "session:tab_close",
    // 文件
    "fs:list",
    "fs:read",
    "fs:read_cancel",
    "fs:write",
    "fs:upload",
    "fs:delete",
    "fs:rename",
    "fs:mkdir",
    // MCP
    "mcp:exec",
    "mcp:exec_start",
    "mcp:exec_input",
    "mcp:exec_close",
    "mcp:exec_list",
    // 桌面控制
    "desktop:start",
    "desktop:stop",
    "desktop:codec",
    "desktop:quality",
    "desktop:bitrate",
    "desktop:gray",
    "desktop:reqkey",
    "desktop:test-delay",
    "desktop:mouse",
    "desktop:key",
    "desktop:clipboard:get",
    "desktop:clipboard:set",
    "desktop:select-display",
    "desktop:qos",
    "desktop:congested",
    // Task 2 P2P 信令（浏览器发起，relay 转发给 agent）
    "desktop:p2p-offer",
    "desktop:p2p-candidate",
    // 网页代理 / 纯 TCP 管道请求
    "web:request",
    "web:cancel",
    "tcp:open",
    "tcp:data",
    "tcp:ack",
    "tcp:eof",
    "tcp:close",
    // 自升级
    "agent:upgrade",
];

/// 该类型是否是已登记的 agent → relay 消息（relay 侧「已知类型」判据）。
pub fn is_known_agent_type(msg_type: &str) -> bool {
    AGENT_TO_RELAY_TYPES.contains(&msg_type)
}

/// 该类型是否是已登记的 relay → agent 消息。
pub fn is_known_relay_type(msg_type: &str) -> bool {
    RELAY_TO_AGENT_TYPES.contains(&msg_type)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// 缺失 → 空串。老 agent 的心跳（`ping`）不带 payload，部分控制消息也
    /// 省略 session_id；让它们能解析，比因为一个可选字段整条丢弃更安全
    /// （消息类型本身仍然必填）。
    #[serde(default)]
    pub session_id: String,
    /// 缺失 → `Value::Null`。所有消费方都用 `payload["k"].as_str()` 之类的
    /// 取值方式，`Null` 上取键得 `None`、带默认值，行为与空对象一致。
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalInputPayload {
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalOutputPayload {
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalResizePayload {
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub size: u64,
    pub mode: String,
    pub owner: String,
    /// Whether the entry itself is a symlink (checked with `symlink_metadata`,
    /// so a link to a directory keeps `type: "directory"` but reports true).
    /// Defaults to false for older agents that don't send the field.
    #[serde(default)]
    pub is_symlink: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsResultPayload {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Coarse error category for HTTP status mapping: not_found |
    /// is_directory | invalid_path | permission_denied | other (absent = other).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<FileEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpExecPayload {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpResultPayload {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecStartPayload {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecInputPayload {
    pub exec_id: String,
    pub data_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecClosePayload {
    pub exec_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecListPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecResultPayload {
    pub exec_id: String,
    pub stdout: String,
    pub stderr: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _mcp_request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecSessionInfo {
    pub exec_id: String,
    pub cmd: String,
    pub status: String,
    pub started_at: u64,
}

/// Device/agent metadata probed by the agent at startup and reported in
/// `agent:register`. All fields are best-effort — an absent/unknowable value
/// is `None` (older agents omit the whole object). Used by the admin device
/// management panel.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// "linux" | "macos" | "windows" | ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// CPU architecture as reported by `uname -m` / PROCESSOR_ARCHITECTURE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// OS identity (e.g. "Linux", "Darwin", "Windows").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Kernel release (e.g. "5.15.0-86-generic").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// CPU model name (e.g. "Intel(R) Xeon(R) Platinum 8375C").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
}

impl DeviceInfo {
    /// True when no field carries a value (nothing was probed).
    pub fn is_empty(&self) -> bool {
        self.hostname.is_none()
            && self.platform.is_none()
            && self.arch.is_none()
            && self.os.is_none()
            && self.kernel.is_none()
            && self.cpu_model.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserInfo {
    pub user_id: String,
    pub permission: Permission,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Permission {
    ReadWrite,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum TokenType {
    Rw,
    Ro,
    Both,
}

impl TokenType {
    pub fn as_str(&self) -> &str {
        match self {
            TokenType::Rw => "rw",
            TokenType::Ro => "ro",
            TokenType::Both => "both",
        }
    }

    pub fn from_str_val(s: &str) -> Option<Self> {
        match s {
            "rw" => Some(TokenType::Rw),
            "ro" => Some(TokenType::Ro),
            "both" => Some(TokenType::Both),
            _ => None,
        }
    }
}

pub fn requires_write(msg_type: &str) -> bool {
    // 只读会话（`--token-type ro` / 后台降权的 token）允许发送的类型。
    //
    // 只列**浏览器确实会发、且不改动远端状态**的类型：
    // - `fs:mkdir` 会创建目录，属写操作 → 不在表内；
    // - `terminal:output` / `fs:result` / `mcp:result` / `mcp:exec_result` 是
    //   agent→relay 方向的回包，浏览器从不发送（已 grep `web/` 确认），
    //   列在只读白名单里等于给只读 token 开了一条伪造结果回包的口子。
    let read_only_types = [
        "session:users",
        "session:tab_list",
        "fs:list",
        "fs:read",
        "mcp:exec_list",
        // R5 授权细化：只读观看会话允许的桌面**只读**操作——关键帧请求
        // （弱网/丢帧恢复）、TestDelay 探针（只读网络测量）、剪贴板读取。
        // 控制类（mouse/key/clipboard:set/quality/codec/gray）仍要求写权限。
        "desktop:reqkey",
        "desktop:test-delay",
        "desktop:clipboard:get",
        // Task 2 P2P 信令：浏览器→agent 的 offer/candidate 不改变桌面状态，
        // 只读观看者也能发起协商（answer/state 是 agent→浏览器方向，本就
        // 不经 requires_write，加入为了对称与可测性）。
        "desktop:p2p-offer",
        "desktop:p2p-answer",
        "desktop:p2p-candidate",
        "desktop:p2p-state",
    ];
    if read_only_types.contains(&msg_type) {
        return false;
    }
    true
}

/// Validate a client-supplied custom session id: 5-20 ASCII alphanumeric
/// chars. Used by both the agent (CLI validation) and the relay (register
/// validation). No regex dependency — a byte loop.
pub fn is_valid_custom_session_id(s: &str) -> bool {
    let len = s.len();
    (5..=20).contains(&len) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

// ── desktop:video 二进制帧格式（R5 #41 线协议的最小可验证子集）──
//
// 目标格式（对齐 rustdesk 逐帧 binary+头{len,seq,flags}）：
//   [flags:u8][seq:u32 LE][len:u32 LE][payload len 字节]
// - WS binary 帧边界自带长度，`len` 再显式携带做**双重校验**（截断/损坏
//   帧拒绝，不吞错）。
// - flags 位 0：1 = key frame（IDR），0 = delta。
// - payload = 原始 fMP4 init/frag 字节（与浏览器桌面 WS 下行同构，未来
//   binary 化 agent→relay 段时直接字节直转，消除 base64 ≈33% 膨胀）。
//
// **当前状态**：格式规格 + 双向编解码已落地并单测（agent/relay 可复用）；
// 实时传输仍走 JSON+base64（第 52 轮帧头字段显式化 seq/flags 的 JSON 等价
// 已就绪，relay 第 53/55 轮观测/丢帧检测已就绪）——agent→relay 段真正
// binary 化（agent 发 binary WS 帧 + relay Binary 分支直转）为架构级远期，
// 本格式是其同构迁移目标。
pub const BIN_FRAME_FLAG_KEY: u8 = 0x01;
/// 位 1：init 段（fMP4 ftyp/moov）。init 帧与 delta/key 独立，relay 收到
/// init 帧走 set_init（重放给新加入 viewer），其余走 push_frag。
pub const BIN_FRAME_FLAG_INIT: u8 = 0x02;
/// 帧头长度：[flags(1) + seq(4) + len(4)]。
pub const BIN_FRAME_HEADER_LEN: usize = 9;

/// 编码 desktop:video 二进制帧。
pub fn encode_bin_frame(flags: u8, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(BIN_FRAME_HEADER_LEN + payload.len());
    out.push(flags);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// 解码 desktop:video 二进制帧；帧头非法 / `len` 与剩余字节不符 → None
/// （不 panic，relay 对损坏帧直接丢弃）。
pub fn decode_bin_frame(bytes: &[u8]) -> Option<(u8, u32, &[u8])> {
    if bytes.len() < BIN_FRAME_HEADER_LEN {
        return None;
    }
    let flags = bytes[0];
    let seq = u32::from_le_bytes(bytes[1..5].try_into().ok()?);
    let len = u32::from_le_bytes(bytes[5..9].try_into().ok()?) as usize;
    if bytes.len() != BIN_FRAME_HEADER_LEN + len {
        return None;
    }
    Some((flags, seq, &bytes[BIN_FRAME_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_custom_session_id() {
        assert!(is_valid_custom_session_id("mydev01"));
        assert!(is_valid_custom_session_id("abcde"));
        assert!(is_valid_custom_session_id("a1b2c3d4e5f6g7h8i9j0")); // exactly 20
                                                                     // too short
        assert!(!is_valid_custom_session_id("ab"));
        assert!(!is_valid_custom_session_id("abcd")); // 4
                                                      // too long
        assert!(!is_valid_custom_session_id("a1b2c3d4e5f6g7h8i9j0k")); // 21
                                                                       // non-alphanumeric
        assert!(!is_valid_custom_session_id("ab cd")); // space
        assert!(!is_valid_custom_session_id("a-b")); // hyphen
        assert!(!is_valid_custom_session_id("dev_01")); // underscore
        assert!(!is_valid_custom_session_id("你好你好你好")); // non-ascii
                                                              // empty
        assert!(!is_valid_custom_session_id(""));
    }

    #[test]
    fn test_message_roundtrip() {
        let msg = Message {
            msg_type: "terminal:input".to_string(),
            session_id: "abc-123".to_string(),
            payload: serde_json::json!({"data": "aGVsbG8="}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.msg_type, "terminal:input");
        assert_eq!(decoded.session_id, "abc-123");
        assert_eq!(decoded.payload["data"].as_str().unwrap(), "aGVsbG8=");
    }

    /// MYS-2553 兼容隐患 a：`session_id` / `payload` 是可选字段。agent 心跳
    /// `{"type":"ping", ...}` 这类消息没有 payload，旧版严格解析会整条失败。
    /// 缺字段时分别取空串 / `Null`，`type` 仍然必填。
    #[test]
    fn test_message_parses_without_payload_or_session_id() {
        // 缺 payload → payload == Null，session_id == ""。
        let decoded: Message = serde_json::from_str(r#"{"type":"ping"}"#).unwrap();
        assert_eq!(decoded.msg_type, "ping");
        assert_eq!(decoded.session_id, "");
        assert!(decoded.payload.is_null(), "缺失的 payload 必须是 Null");
        // 在 Null 上按 key 取值不会 panic，行为与空对象一致。
        assert!(decoded.payload["data"].as_str().is_none());
        assert_eq!(decoded.payload.get("data").and_then(|v| v.as_str()), None);

        // 缺 session_id 但带 payload。
        let decoded: Message =
            serde_json::from_str(r#"{"type":"desktop:state","payload":{"running":true}}"#)
                .unwrap();
        assert_eq!(decoded.session_id, "");
        assert_eq!(decoded.payload["running"].as_bool(), Some(true));

        // `type` 仍然必填——没有它就不知道该消息是什么。
        assert!(serde_json::from_str::<Message>(r#"{"session_id":"s1","payload":{}}"#).is_err());
    }

    /// MYS-2553：协议版本解析对非法值一律回落到 0（旧版本），绝不报错。
    #[test]
    fn test_parse_protocol_version() {
        assert_eq!(parse_protocol_version(None), 0);
        assert_eq!(parse_protocol_version(Some(&serde_json::json!(1))), 1);
        assert_eq!(parse_protocol_version(Some(&serde_json::json!(0))), 0);
        // 非法：负数 / 字符串 / 浮点 / null / 对象 → 0
        for bad in [
            serde_json::json!(-1),
            serde_json::json!("1"),
            serde_json::json!(1.5),
            serde_json::json!(null),
            serde_json::json!({"v": 1}),
        ] {
            assert_eq!(parse_protocol_version(Some(&bad)), 0, "{bad} 必须回落为 0");
        }
    }

    /// MYS-2553 设计 3d：消息类型注册表是**唯一**登记处。这里守住两条：
    /// 1. 漏登记过的 P2P 回包必须在表里（它们本就被广播，此前每次都被当成
    ///    unknown 打日志）；
    /// 2. 两个方向不重复、无空项，且同一类型不会同时出现在两个方向
    ///    （唯一例外 `tcp:data`/`tcp:ack`/`tcp:eof` 是真正的双向消息）。
    #[test]
    fn test_message_type_registry() {
        assert!(
            is_known_agent_type("desktop:p2p-answer"),
            "desktop:p2p-answer 必须登记为 agent→relay（此前漏登记，每条白打 unknown 日志）"
        );
        assert!(
            is_known_agent_type("desktop:p2p-state"),
            "desktop:p2p-state 必须登记为 agent→relay"
        );
        assert!(is_known_relay_type("desktop:p2p-offer"));
        assert!(is_known_relay_type("desktop:p2p-candidate"));
        // agent 侧应答类消息不得被误登记成 relay→agent。
        assert!(!is_known_relay_type("desktop:p2p-answer"));
        assert!(!is_known_relay_type("desktop:p2p-state"));

        // 无重复、无空项。
        for (name, list) in [
            ("AGENT_TO_RELAY_TYPES", AGENT_TO_RELAY_TYPES),
            ("RELAY_TO_AGENT_TYPES", RELAY_TO_AGENT_TYPES),
        ] {
            let mut seen = std::collections::HashSet::new();
            for t in list {
                assert!(!t.is_empty(), "{name} 含空项");
                assert!(seen.insert(*t), "{name} 重复登记 {t}");
            }
        }

        // 两个方向的交集只能是真正的双向消息：请求与回包/广播共用同一个
        // 类型名（`tcp:*` 是管道两个方向共用；`fs:mkdir` 既是浏览器→agent
        // 的建目录请求，也是 agent→浏览器广播的「目录已建」事件）。
        let bidir = ["tcp:data", "tcp:ack", "tcp:eof", "fs:mkdir"];
        for t in AGENT_TO_RELAY_TYPES {
            if RELAY_TO_AGENT_TYPES.contains(t) {
                assert!(bidir.contains(t), "{t} 同时出现在两个方向，但没有登记为双向消息");
            }
        }

        // 未知类型不命中任何一个方向（relay 据此打 unknown 日志）。
        assert!(!is_known_agent_type("agent:registered"));
        assert!(!is_known_agent_type("browser:connected"));
        assert!(!is_known_relay_type("agent:registered"));
    }

    /// `requires_write` 的只读白名单全是浏览器可发的类型，必须都能在
    /// relay→agent 注册表里找到（防止新增只读类型时忘了登记方向）。
    #[test]
    fn test_read_only_types_are_registered() {
        for t in [
            "session:users",
            "session:tab_list",
            "fs:list",
            "fs:read",
            "mcp:exec_list",
            "desktop:reqkey",
            "desktop:test-delay",
            "desktop:clipboard:get",
            "desktop:p2p-offer",
            "desktop:p2p-answer",
            "desktop:p2p-candidate",
            "desktop:p2p-state",
        ] {
            assert!(
                is_known_relay_type(t) || is_known_agent_type(t),
                "{t} 未登记进任一方向的类型表"
            );
        }
    }

    #[test]
    fn test_terminal_output_roundtrip() {
        let output = TerminalOutputPayload {
            data: "SGVsbG8gV29ybGQ=".to_string(),
            tab_id: Some("tab-1".to_string()),
        };
        let msg = Message {
            msg_type: "terminal:output".to_string(),
            session_id: "session-1".to_string(),
            payload: serde_json::to_value(&output).unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        let decoded_output: TerminalOutputPayload =
            serde_json::from_value(decoded.payload).unwrap();
        assert_eq!(decoded_output.data, "SGVsbG8gV29ybGQ=");
    }

    #[test]
    fn test_fs_result_roundtrip() {
        let entry = FileEntry {
            name: "test.txt".to_string(),
            path: "/home/user/test.txt".to_string(),
            entry_type: "file".to_string(),
            size: 1024,
            mode: "-rw-r--r--".to_string(),
            owner: "1000:1000".to_string(),
            is_symlink: false,
        };
        let result = FsResultPayload {
            success: true,
            error: None,
            kind: None,
            entries: Some(vec![entry]),
            content: None,
            path: None,
            new_path: None,
        };
        let msg = Message {
            msg_type: "fs:result".to_string(),
            session_id: "session-1".to_string(),
            payload: serde_json::to_value(&result).unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        let decoded_result: FsResultPayload = serde_json::from_value(decoded.payload).unwrap();
        assert!(decoded_result.success);
        assert_eq!(decoded_result.entries.unwrap().len(), 1);
    }

    #[test]
    fn test_requires_write() {
        assert!(requires_write("terminal:input"));
        assert!(requires_write("fs:write"));
        assert!(requires_write("fs:delete"));
        assert!(requires_write("fs:mkdir"), "创建目录是写操作（只读 token 不得创建目录）");
        // agent→relay 的回包类型不属于浏览器可发集合 → 非只读（fail-closed）。
        assert!(requires_write("terminal:output"));
        assert!(requires_write("fs:result"));
        assert!(requires_write("mcp:result"));
        assert!(requires_write("mcp:exec_result"));
        assert!(requires_write("session:join")); // unknown type → fail-closed → requires write
        assert!(!requires_write("fs:list"));
        assert!(!requires_write("fs:read"));
        assert!(!requires_write("session:users"));
        // R5 授权细化：只读桌面操作允许（ro 观看可 reqkey 恢复/TestDelay 测量/
        // 剪贴板读取）；控制类仍要求写。
        assert!(!requires_write("desktop:reqkey"));
        assert!(!requires_write("desktop:test-delay"));
        assert!(!requires_write("desktop:clipboard:get"));
        // Task 2 P2P 信令：浏览器→agent 的 p2p-offer/candidate 不应被只读
        // 观看者 403 阻断（协商不改变桌面状态）；agent→浏览器 的 answer/state
        // 本就经广播，加入保持对称。
        assert!(!requires_write("desktop:p2p-offer"));
        assert!(!requires_write("desktop:p2p-answer"));
        assert!(!requires_write("desktop:p2p-candidate"));
        assert!(!requires_write("desktop:p2p-state"));
        assert!(requires_write("desktop:mouse"));
        assert!(requires_write("desktop:key"));
        assert!(requires_write("desktop:clipboard:set"));
        assert!(requires_write("desktop:quality"));
        assert!(requires_write("unknown:whatever")); // unknown → fail-closed
    }

    #[test]
    fn test_bin_frame_roundtrip() {
        // R5 #41 格式规格：key/delta/init 帧 roundtrip，seq 边界与空 payload。
        for flags in [0u8, BIN_FRAME_FLAG_KEY, BIN_FRAME_FLAG_INIT, BIN_FRAME_FLAG_KEY | BIN_FRAME_FLAG_INIT] {
            let payload = vec![1u8, 2, 3, 4];
            let bytes = encode_bin_frame(flags, 42, &payload);
            assert_eq!(bytes.len(), BIN_FRAME_HEADER_LEN + 4);
            let (f, s, p) = decode_bin_frame(&bytes).unwrap();
            assert_eq!(f, flags);
            assert_eq!(s, 42);
            assert_eq!(p, payload.as_slice());
        }
        // seq 边界：0 与 u32::MAX。
        let b = encode_bin_frame(0, 0, &[]);
        let (_, s, p) = decode_bin_frame(&b).unwrap();
        assert_eq!(s, 0);
        assert!(p.is_empty());
        let b = encode_bin_frame(0, u32::MAX, b"x");
        let (_, s, p) = decode_bin_frame(&b).unwrap();
        assert_eq!(s, u32::MAX);
        assert_eq!(p, b"x");
    }

    #[test]
    fn test_bin_frame_rejects_malformed() {
        // 拒绝：<9 字节（头不全）、len 与剩余不符（截断/多出）、
        // len 声称超大但字节不足。
        assert!(decode_bin_frame(&[]).is_none());
        assert!(decode_bin_frame(&[0]).is_none());
        assert!(decode_bin_frame(&[0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        // len=4 但只有 2 个 payload 字节 → 长度不符拒绝。
        let mut short = encode_bin_frame(0, 1, b"abcd");
        short.pop();
        short.pop();
        assert!(decode_bin_frame(&short).is_none());
        // len=4 但 payload 多出 1 字节 → 拒绝。
        let mut extra = encode_bin_frame(0, 1, b"abcd");
        extra.push(0xff);
        assert!(decode_bin_frame(&extra).is_none());
        // 头内 len 被篡改为超大值 → 拒绝（不 panic）。
        let mut tampered = encode_bin_frame(0, 1, b"ab");
        tampered[5] = 0xff;
        tampered[6] = 0xff;
        tampered[7] = 0xff;
        tampered[8] = 0x7f;
        assert!(decode_bin_frame(&tampered).is_none());
    }

    #[test]
    fn test_error_payload_roundtrip() {
        let err = ErrorPayload {
            code: "AUTH_INVALID_TOKEN".to_string(),
            message: "Invalid token".to_string(),
        };
        let msg = Message {
            msg_type: "error".to_string(),
            session_id: "session-1".to_string(),
            payload: serde_json::to_value(&err).unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        let decoded_err: ErrorPayload = serde_json::from_value(decoded.payload).unwrap();
        assert_eq!(decoded_err.code, "AUTH_INVALID_TOKEN");
    }

    #[test]
    fn test_mcp_exec_roundtrip() {
        let exec = McpExecPayload {
            cmd: "ls -la".to_string(),
            timeout_ms: Some(5000),
        };
        let json = serde_json::to_string(&exec).unwrap();
        let decoded: McpExecPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cmd, "ls -la");
        assert_eq!(decoded.timeout_ms, Some(5000));
    }

    #[test]
    fn test_mcp_result_roundtrip() {
        let result = McpResultPayload {
            stdout: "file.txt".to_string(),
            stderr: String::new(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: McpResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stdout, "file.txt");
        assert_eq!(decoded.exit_code, 0);
    }

    #[test]
    fn test_exec_start_roundtrip() {
        let payload = ExecStartPayload {
            cmd: "sudo apt update".to_string(),
            _mcp_request_id: Some("req-1".to_string()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: ExecStartPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cmd, "sudo apt update");
        assert_eq!(decoded._mcp_request_id, Some("req-1".to_string()));
    }

    #[test]
    fn test_exec_result_roundtrip() {
        let payload = ExecResultPayload {
            exec_id: "abc123".to_string(),
            stdout: "output".to_string(),
            stderr: String::new(),
            status: "running".to_string(),
            exit_code: None,
            error: None,
            _mcp_request_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: ExecResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.exec_id, "abc123");
        assert_eq!(decoded.status, "running");
        assert_eq!(decoded.exit_code, None);
    }

    #[test]
    fn test_exec_result_exited_roundtrip() {
        let payload = ExecResultPayload {
            exec_id: "abc123".to_string(),
            stdout: "done\n".to_string(),
            stderr: String::new(),
            status: "exited".to_string(),
            exit_code: Some(0),
            error: None,
            _mcp_request_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: ExecResultPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, "exited");
        assert_eq!(decoded.exit_code, Some(0));
    }

    #[test]
    fn test_exec_session_info_roundtrip() {
        let info = ExecSessionInfo {
            exec_id: "abc123".to_string(),
            cmd: "sleep 10".to_string(),
            status: "running".to_string(),
            started_at: 1718300000,
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: ExecSessionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.exec_id, "abc123");
        assert_eq!(decoded.cmd, "sleep 10");
        assert_eq!(decoded.status, "running");
    }

    #[test]
    fn test_exec_start_cmd_only() {
        let payload = ExecStartPayload {
            cmd: "ls".to_string(),
            _mcp_request_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("_mcp_request_id"));
        let decoded: ExecStartPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cmd, "ls");
    }

    #[test]
    fn test_device_info_roundtrip() {
        let info = DeviceInfo {
            hostname: Some("dev-01".to_string()),
            platform: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            os: Some("Linux".to_string()),
            kernel: Some("5.15.0-86-generic".to_string()),
            cpu_model: Some("Intel(R) Xeon(R) Platinum 8375C".to_string()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: DeviceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, info);
        assert!(!decoded.is_empty());
    }

    #[test]
    fn test_device_info_default_empty() {
        let info = DeviceInfo::default();
        assert!(info.is_empty());
        // An empty device object serializes to `{}` and roundtrips to the
        // same default — the relay can always expect a valid DeviceInfo.
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(json, "{}");
        let decoded: DeviceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, DeviceInfo::default());
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_device_info_partial_fields() {
        // Older agents / partial probes leave missing fields as None but keep
        // the populated ones — the admin panel must not choke.
        let json = r#"{"hostname":"box1","arch":"aarch64"}"#;
        let decoded: DeviceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.hostname.as_deref(), Some("box1"));
        assert_eq!(decoded.arch.as_deref(), Some("aarch64"));
        assert_eq!(decoded.cpu_model, None);
        assert_eq!(decoded.platform, None);
        assert!(!decoded.is_empty());
    }
}
