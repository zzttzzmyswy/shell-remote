//! 冻结的协议金样（golden fixtures）与回放/契约/覆盖测试（MYS-2553 兼容 2）。
//!
//! # 为什么要有夹具
//!
//! 兼容性不能只靠"读代码时小心"。这里把每个协议真实版本下双方发出的消息录成
//! JSON 文件，`cargo test` 逐条回放：**当前代码必须能处理所有历史版本的夹具**。
//! 破坏兼容的改动在 CI 这一层就被拦住，不依赖人工 review 有没有想到。
//!
//! 红线：**已发布版本的夹具不允许修改**。`FROZEN.sha256` 记下每个夹具的
//! sha256，测试逐条校验；内容一变就失败，提示信息写明该怎么做。要变更协议，
//! 增一个 `fixtures/v<N+1>/` 目录，旧版本夹具必须继续通过。
//!
//! # 目录布局
//!
//! ```text
//! src/proto/fixtures/
//!   FROZEN.sha256              # 冻结清单：<sha256>  <相对路径>
//!   v0/                        # 协议版本标记出现之前的线协议（≤ v0.59.11）
//!     agent_to_relay/*.json
//!     relay_to_agent/*.json
//!   v1/                        # 当前线协议（PROTOCOL_VERSION = 1）
//!     agent_to_relay/*.json
//!     relay_to_agent/*.json
//! ```
//!
//! 文件名规则：`<类型里的冒号换成下划线>[__<变体>].json`。例如
//! `terminal_output.json`、`agent_register__reconnect_tokens.json`。
//! 二进制帧（`desktop:video` 的 9 字节头格式）存成 `.bin`。
//!
//! 一个文件承载一条**完整信封**（`{type, session_id, payload}` + 该版本特有的
//! 顶层字段），因此回放测试可以直接把它喂给真实的路由/解析代码。
//!
//! # 怎么重新生成
//!
//! 夹具内容**从真实代码路径产出**，不手写：生成器调用生产代码里的构造函数
//! （`agent::client::build_register_message`、`relay::ws::build_register_response`、
//! `relay::admin::build_upgrade_message`、`relay::mcp::build_mcp_exec_message`、
//! `relay::file_transfer::build_fs_read_message` 等），或直接复刻各模块里
//! `json!({...})` 的字段集（那些点没有抽成函数）。生成走 `#[ignore]` 测试：
//!
//! ```text
//! REGEN_FIXTURES=1 cargo test --lib proto::fixtures::tests::regen_fixtures -- --ignored --nocapture
//! ```
//!
//! 它会重写 `v1/` 下的全部文件和 `FROZEN.sha256`。**`v0/` 永远不重新生成**——
//! 那是已发布的线协议，改动等于伪造历史；`v0/` 的文件由手工一次性落盘后就被
//! 冻结清单钉住。
//!
//! # 版本升级时怎么冻结
//!
//! 详见 `docs/protocol.md` 的「夹具与冻结规则」一节。

use serde_json::Value;

/// 一个协议版本的夹具目录名。
pub const V0: &str = "v0";
pub const V1: &str = "v1";

/// 夹具的绝对定位基准：编译期把 `src/proto/fixtures` 目录嵌进来。
///
/// 用 `env!("CARGO_MANIFEST_DIR")` 而不是 `CARGO_MANIFEST_DIR` 运行期读取，
/// 保证在任意 cwd 下跑测试都能找到夹具（`cargo test` 的 cwd 不一定是仓库根）。
pub const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/proto/fixtures");

/// 冻结清单文件名。
pub const FROZEN_FILE: &str = "FROZEN.sha256";

/// 夹具文件名：把消息类型里的冒号换成下划线，可选变体后缀。
///
/// `terminal:output` → `terminal_output.json`；
/// `agent:register` + `reconnect_tokens` →
/// `agent_register__reconnect_tokens.json`。
pub fn fixture_filename(msg_type: &str, variant: Option<&str>) -> String {
    let stem = msg_type.replace(':', "_");
    match variant {
        Some(v) => format!("{stem}__{v}.json"),
        None => format!("{stem}.json"),
    }
}

/// 读取一个夹具文件（相对 `src/proto/fixtures` 的路径）。
///
/// 不存在时 panic 并给出可操作的提示：缺少夹具通常是「新增了消息类型但忘了
/// 补夹具」，或路径拼错。
pub fn read_fixture(rel_path: &str) -> String {
    let full = format!("{FIXTURE_DIR}/{rel_path}");
    std::fs::read_to_string(&full).unwrap_or_else(|e| {
        panic!(
            "读不到协议夹具 {full}: {e}\n\
             新增消息类型必须补夹具（对应版本、对应方向的目录下放一个 JSON），\
             见 docs/protocol.md「夹具与冻结规则」。"
        )
    })
}

/// 读取并解析一个 JSON 夹具。
pub fn read_fixture_json(rel_path: &str) -> Value {
    let text = read_fixture(rel_path);
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("协议夹具 {rel_path} 不是合法 JSON: {e}"))
}

/// 解析冻结清单：`<sha256>  <相对路径>`，每行一条，空行与 `#` 注释忽略。
pub fn parse_frozen(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?.to_string();
            let path = parts.next()?.to_string();
            Some((hash, path))
        })
        .collect()
}

/// 计算一段字节的 sha256（小写 hex）。与冻结清单同格式。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 列出一个目录下所有夹具文件名（相对该目录），升序，跳过子目录。
pub fn list_fixtures(version: &str, direction: &str) -> Vec<String> {
    let dir = format!("{FIXTURE_DIR}/{version}/{direction}");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读不到夹具目录 {dir}: {e}"))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            entry.file_name().into_string().ok()
        })
        .collect();
    names.sort();
    names
}

/// 夹具路径（相对 `src/proto/fixtures`）。
pub fn fixture_rel_path(version: &str, direction: &str, name: &str) -> String {
    format!("{version}/{direction}/{name}")
}

/// 两个方向的目录名，与 `docs/protocol.md` 的口径一致。
pub const AGENT_TO_RELAY: &str = "agent_to_relay";
pub const RELAY_TO_AGENT: &str = "relay_to_agent";

/// 协议版本目录的升序列表——回放测试对**所有版本**跑同一套断言。
///
/// 新增 `v2/` 时只改这里，回放/契约测试自动把它纳入（这正是"当前代码必须能
/// 处理所有历史版本"的落点）。
pub const ALL_VERSIONS: &[&str] = &[V0, V1];

/// 当前正在开发（也即最新）的协议版本目录。覆盖率断言只要求它齐全——
/// 旧版本是历史，不要求补齐当时就没有的类型。
pub const CURRENT_VERSION_DIR: &str = V1;

// ── 输出契约：旧版本对端要求的字段清单（MYS-2553 兼容 2）──────────────
//
// 这四份清单是「不许删」的字段白名单，与 `docs/protocol.md` §6 逐条对应。
// 测试断言的是**当前代码实际发出的消息包含这些字段**，防止有人顺手删掉一个
// 「看起来没人用」的键——旧版本的对端只认那些键，删了它们老设备就静默坏掉。
//
// 清单只列**旧对端会读**的字段；新增字段不在此列（新增是允许的，不需要报备）。

/// relay 发出的 `agent:registered` 必须带的顶层字段。
///
/// `desktop_binary` 是 v0 旧 agent 判断二进制上行的唯一依据；
/// `payload.tokens` 是它拿身份令牌的唯一来源。
pub const CONTRACT_AGENT_REGISTERED: &[&str] = &[
    "type",
    "session_id",
    "evicted",
    "desktop_binary",
    "protocol_version",
    "relay_version",
    "relay_capabilities",
    "payload",
];

/// `agent:registered.payload` 必须带的字段。
pub const CONTRACT_AGENT_REGISTERED_PAYLOAD: &[&str] = &["tokens"];

/// relay 发出的 `agent:upgrade.payload` 必须带的字段（v0 旧 agent 只认
/// `version` / `url` / `sha256` 三个键；新 agent 还要 target_version 等）。
pub const CONTRACT_AGENT_UPGRADE_PAYLOAD: &[&str] = &[
    "target_version",
    "version",
    "filename",
    "size",
    "sha256",
    "relay_url",
    "url",
    "relay_fallback",
    "source",
];

/// relay 发出的 `mcp:exec.payload` 必须带的字段。
pub const CONTRACT_MCP_EXEC_PAYLOAD: &[&str] = &["cmd", "timeout_ms", "_mcp_request_id"];

/// relay 发出的 `fs:read.payload` 必须带的字段。
pub const CONTRACT_FS_READ_PAYLOAD: &[&str] = &["path", "_mcp_request_id"];

/// agent 发出的 `agent:register` 必须带的顶层字段（v0 relay 会读
/// `key` / `token_type` / `tokens` / `agent_version` / `capabilities` /
/// `device` / `lan_addr`；其中 `key`/`token_type` 与 `tokens` 二选一）。
pub const CONTRACT_AGENT_REGISTER: &[&str] = &[
    "type",
    "agent_version",
    "capabilities",
    "device",
    "protocol_version",
];

/// 该版本目录下某个方向的夹具数量。
pub fn count_fixtures(version: &str, direction: &str) -> usize {
    list_fixtures(version, direction).len()
}

/// 「注册类」夹具的判定：文件名以 `agent_register` 开头。
///
/// 回放测试靠它把注册请求和普通消息分开断言（注册要得到 `agent:registered`，
/// 普通消息要走 `route_agent_message`）。
pub fn is_register_fixture(name: &str) -> bool {
    name.starts_with("agent_register")
}

/// 从夹具文件读出它承载的**真实**消息类型（信封里的 `type` 字段）。
///
/// 不从文件名反推：`desktop_cmd-ack.json` 之类带连字符的类型没法无歧义还原，
/// 而信封里的 `type` 就是线上那一条。`.bin` 夹具（`desktop:video` 二进制帧）
/// 没有信封，按命名约定返回 `desktop:video`。
pub fn msg_type_of_fixture(version: &str, direction: &str, name: &str) -> String {
    if name.ends_with(".bin") {
        return "desktop:video".to_string();
    }
    let rel = fixture_rel_path(version, direction, name);
    read_fixture_json(&rel)
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("夹具 {rel} 的顶层没有字符串 `type` 字段"))
        .to_string()
}

/// 某版本某方向下出现过的全部消息类型（去重、升序）。
pub fn fixture_types(version: &str, direction: &str) -> Vec<String> {
    let mut types: Vec<String> = list_fixtures(version, direction)
        .iter()
        .map(|name| msg_type_of_fixture(version, direction, name))
        .collect();
    types.sort();
    types.dedup();
    types
}

#[cfg(test)]
#[path = "fixtures_tests.rs"]
mod tests;

/// 夹具生成器：从真实代码路径产出 `v1/` 夹具。仅测试构建编译（它调用的
/// 构造函数里有 `pub(crate)` 的，而且生产进程永远不需要写夹具）。
#[cfg(test)]
#[path = "fixtures_gen.rs"]
pub mod gen;
