//! 冻结金样的校验 / 回放 / 契约 / 覆盖测试（MYS-2553 兼容 2）。
//!
//! 四组：
//! 1. **冻结**：`FROZEN.sha256` 里每个夹具的 sha256 与磁盘一致，且清单不缺项。
//! 2. **回放**：所有版本的夹具喂进真实路由/解析代码，断言当前代码能正确处理。
//! 3. **输出契约**：当前 relay/agent 发出的关键消息必须包含旧版本对端要求的
//!    全部字段（清单见 `docs/protocol.md` §6）。
//! 4. **覆盖率**：注册表里每个类型在最新版本目录下都有夹具。

use serde_json::{json, Value};

use super::*;
use crate::proto::{AGENT_TO_RELAY_TYPES, RELAY_TO_AGENT_TYPES};

// ── 1. 冻结 ────────────────────────────────────────────────────────

/// 冻结清单与磁盘上的夹具必须一一对应、逐字节一致。
///
/// 这是整套机制的守门人：夹具一改，这里就红。提示信息里写清正确做法
/// （新增 `v<N+1>/`），因为看到这条失败的人第一反应通常是「就地改夹具」。
#[test]
fn test_frozen_fixtures_are_unmodified() {
    let frozen_rel = FROZEN_FILE;
    let text = read_fixture(frozen_rel);
    let entries = parse_frozen(&text);
    assert!(
        !entries.is_empty(),
        "{frozen_rel} 不能是空的——否则冻结机制形同虚设"
    );

    let mut problems: Vec<String> = Vec::new();
    for (expected, rel) in &entries {
        let full = format!("{FIXTURE_DIR}/{rel}");
        let bytes = match std::fs::read(&full) {
            Ok(b) => b,
            Err(e) => {
                problems.push(format!("{rel}：清单里有，但文件读不到（{e}）"));
                continue;
            }
        };
        let actual = sha256_hex(&bytes);
        if &actual != expected {
            problems.push(format!(
                "{rel}：内容已被改动（清单 {expected}，实际 {actual}）"
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "已发布协议版本的夹具不可修改：要变更协议，请新增 `fixtures/v<N+1>/`，\
         并保持旧版本夹具继续通过。见 docs/protocol.md「夹具与冻结规则」。\n\
         发现 {} 处问题：\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

/// 磁盘上的每个夹具都必须在冻结清单里（防止新增了夹具却忘了登记，让它可以
/// 被随手改掉）。
#[test]
fn test_every_fixture_is_frozen() {
    let entries = parse_frozen(&read_fixture(FROZEN_FILE));
    let listed: std::collections::HashSet<&str> =
        entries.iter().map(|(_, p)| p.as_str()).collect();

    let mut unlisted: Vec<String> = Vec::new();
    for version in ALL_VERSIONS {
        for direction in [AGENT_TO_RELAY, RELAY_TO_AGENT] {
            for name in list_fixtures(version, direction) {
                let rel = fixture_rel_path(version, direction, &name);
                if !listed.contains(rel.as_str()) {
                    unlisted.push(rel);
                }
            }
        }
    }
    assert!(
        unlisted.is_empty(),
        "这些夹具没登记进 {FROZEN_FILE}，等于没被冻结：\n  {}\n\
         重新跑生成器（见 src/proto/fixtures.rs 的模块文档）会把清单补齐。",
        unlisted.join("\n  ")
    );
}

/// 冻结清单里不能有指向已删除文件的僵尸条目。
#[test]
fn test_frozen_manifest_has_no_stale_entries() {
    let entries = parse_frozen(&read_fixture(FROZEN_FILE));
    let missing: Vec<&str> = entries
        .iter()
        .filter(|(_, rel)| !std::path::Path::new(&format!("{FIXTURE_DIR}/{rel}")).is_file())
        .map(|(_, rel)| rel.as_str())
        .collect();
    assert!(
        missing.is_empty(),
        "{FROZEN_FILE} 里有已不存在的夹具（删除夹具同样属于破坏历史）：\n  {}",
        missing.join("\n  ")
    );
}

/// 每个版本 × 方向目录都必须非空——空目录会让回放测试「通过」却什么都没验。
#[test]
fn test_every_version_direction_has_fixtures() {
    for version in ALL_VERSIONS {
        for direction in [AGENT_TO_RELAY, RELAY_TO_AGENT] {
            let n = count_fixtures(version, direction);
            assert!(
                n > 0,
                "夹具目录 {version}/{direction} 是空的：每个协议版本都必须留下\
                 该方向的真实消息样本"
            );
        }
    }
}

// ── 4. 覆盖率（先写，后面的回放依赖它保证输入完整）────────────────

/// 注册表里每一个 agent→relay 类型，在**当前版本**目录下至少有一个夹具。
///
/// 判据读夹具信封里的 `type`（不是文件名），因此变体文件名不会漏判。
#[test]
fn test_fixture_covers_every_agent_to_relay_type() {
    let covered = fixture_types(CURRENT_VERSION_DIR, AGENT_TO_RELAY);
    let missing: Vec<&&str> = AGENT_TO_RELAY_TYPES
        .iter()
        .filter(|t| !covered.iter().any(|c| c == *t))
        .collect();
    assert!(
        missing.is_empty(),
        "新增消息类型必须补夹具：{CURRENT_VERSION_DIR}/{AGENT_TO_RELAY}/ 下缺少这些类型的\
         样本（一个类型至少一条 JSON，文件名用类型里的冒号换成下划线）:\n  {}\n\
         见 docs/protocol.md「夹具与冻结规则」。",
        missing
            .iter()
            .map(|t| **t)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// 同上，relay→agent 方向。
#[test]
fn test_fixture_covers_every_relay_to_agent_type() {
    let covered = fixture_types(CURRENT_VERSION_DIR, RELAY_TO_AGENT);
    let missing: Vec<&&str> = RELAY_TO_AGENT_TYPES
        .iter()
        .filter(|t| !covered.iter().any(|c| c == *t))
        .collect();
    assert!(
        missing.is_empty(),
        "新增消息类型必须补夹具：{CURRENT_VERSION_DIR}/{RELAY_TO_AGENT}/ 下缺少这些类型的\
         样本（一个类型至少一条 JSON，文件名用类型里的冒号换成下划线）:\n  {}\n\
         见 docs/protocol.md「夹具与冻结规则」。",
        missing
            .iter()
            .map(|t| **t)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// 覆盖率断言必须真的按信封里的 `type` 判定，而不是按文件名——否则
/// `desktop_cmd-ack.json` 这类带连字符的类型会被误判成缺失。
#[test]
fn test_coverage_reads_envelope_type_not_filename() {
    let types = fixture_types(CURRENT_VERSION_DIR, AGENT_TO_RELAY);
    assert!(
        types.iter().any(|t| t == "desktop:cmd-ack"),
        "desktop:cmd-ack 的文件名是 desktop_cmd-ack.json，判定必须读信封 type，\
         实际覆盖集合：{types:?}"
    );
    // .bin 夹具没有信封，按命名约定算作 desktop:video。
    assert!(
        types.iter().any(|t| t == "desktop:video"),
        "二进制帧夹具（.bin）必须算作 desktop:video 的覆盖"
    );
}

// ── 2a. 回放：relay 侧 ────────────────────────────────────────────

/// 起一个用于回放的 relay state（无服务器口令：回放不验鉴权，只验消息处理）。
fn make_state() -> std::sync::Arc<crate::relay::SharedState> {
    std::sync::Arc::new(crate::relay::SharedState::new(
        String::new(),
        100 * 1024 * 1024,
        None,
        String::new(),
        String::new(),
        None,
    ))
}

/// 灌一条消息进 relay 的 `/agent/send`（生产入口），返回 HTTP 状态。
async fn post_to_relay(
    state: &std::sync::Arc<crate::relay::SharedState>,
    body: &Value,
) -> axum::http::StatusCode {
    use axum::response::IntoResponse;
    crate::relay::ws::agent_send_handler(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        axum::Json(body.clone()),
    )
    .await
    .into_response()
    .status()
}

/// 注册一台 agent 并挂上 agent 通道，返回 (session_id, 该通道的接收端)。
///
/// 注册用**生产注册路径**（`agent_send_handler` 的 `agent:register` 分支），
/// 因此后面回放普通消息时走的是真实的「已注册会话」校验。
async fn register_session(
    state: &std::sync::Arc<crate::relay::SharedState>,
) -> (String, tokio::sync::mpsc::Receiver<String>) {
    use axum::response::IntoResponse;
    let resp = crate::relay::ws::agent_send_handler(
        axum::extract::State(state.clone()),
        axum::http::HeaderMap::new(),
        axum::Json(json!({"type": "agent:register", "key": "golden-key", "token_type": "rw"})),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), 200, "注册必须成功");
    let raw = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("注册响应体可读");
    let v: Value = serde_json::from_slice(&raw).expect("注册响应是 JSON");
    assert_eq!(v["type"], "agent:registered");
    let sid = v["session_id"].as_str().expect("session_id").to_string();

    let (tx, rx) = tokio::sync::mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);
    let mut cm = crate::relay::ChannelMap::new();
    cm.agent = Some(tx);
    state.agent_broadcast.write().await.insert(sid.clone(), cm);
    (sid, rx)
}

/// 订阅一个浏览器 SSE 端，返回接收端（`route_agent_message` 会把广播类消息
/// 投到这里）。
async fn subscribe_browser(
    state: &std::sync::Arc<crate::relay::SharedState>,
    sid: &str,
) -> tokio::sync::mpsc::Receiver<String> {
    let user = format!("user-{}", sid);
    let bsid = format!("sse-{}", sid);
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(crate::relay::SSE_CHANNEL_CAPACITY);
    state.sse_sessions.write().await.insert(bsid.clone(), tx);
    let mut broadcast = state.agent_broadcast.write().await;
    if let Some(cm) = broadcast.get_mut(sid) {
        cm.browser_sessions.insert(user, bsid);
    }
    rx
}

/// 回放所有版本的 `agent_to_relay` 夹具中**注册类**的那几个：必须得到
/// `agent:registered`，且版本字段按夹具版本正确落库（v0 夹具 → 协议版本 0）。
#[tokio::test]
async fn test_replay_agent_to_relay_register_fixtures() {
    for version in ALL_VERSIONS {
        for name in list_fixtures(version, AGENT_TO_RELAY) {
            if !is_register_fixture(&name) {
                continue;
            }
            let rel = fixture_rel_path(version, AGENT_TO_RELAY, &name);
            let body = read_fixture_json(&rel);
            let state = make_state();
            let status = post_to_relay(&state, &body).await;
            assert_eq!(
                status, 200,
                "{rel}：注册夹具必须注册成功，实际 HTTP {status}"
            );

            // 注册响应的 session_id 必须能查到会话，且协议版本符合该夹具版本。
            let sid = state
                .sessions
                .list_sessions()
                .await
                .into_iter()
                .map(|(sid, _)| sid)
                .next()
                .unwrap_or_else(|| panic!("{rel}：注册后必须留下一个会话"));
            let info = state.sessions.get(&sid).await.expect("会话必须存在");
            let expected_pv = if *version == V0 { 0 } else { crate::proto::PROTOCOL_VERSION };
            assert_eq!(
                info.agent_protocol_version, expected_pv,
                "{rel}：{version} 夹具的协议版本必须记成 {expected_pv}（v0 = 协议标记出现之前）"
            );
        }
    }
}

/// 回放所有版本的 `agent_to_relay` 普通消息夹具：全部被接受（HTTP 200），
/// 且**不会**被判成未知类型（未知类型在 relay 侧会被丢弃并打 unknown 日志）。
///
/// 「不会判成未知类型」的可观测判据：每一条都在注册表里（`is_known_agent_type`），
/// 这正是 `route_agent_message` 尾部那条 unknown 日志的判据。这条断言把
/// 「v0 夹具的能力不能被新版悄悄取消」钉死。
#[tokio::test]
async fn test_replay_agent_to_relay_plain_fixtures_are_known_types() {
    for version in ALL_VERSIONS {
        for name in list_fixtures(version, AGENT_TO_RELAY) {
            if is_register_fixture(&name) || name.ends_with(".bin") {
                continue;
            }
            let rel = fixture_rel_path(version, AGENT_TO_RELAY, &name);
            let body = read_fixture_json(&rel);
            let ty = body["type"].as_str().expect("夹具必须有 type");
            assert!(
                crate::proto::is_known_agent_type(ty),
                "{rel}：类型 {ty} 不在 AGENT_TO_RELAY_TYPES 里 → relay 会把它当未知\
                 类型丢弃（v0 夹具里的类型都是线上跑过的，不许退化成未知）"
            );

            // 真实投递：注册一台 agent，把 session_id 换成它自己的，再 POST。
            let state = make_state();
            let (sid, _agent_rx) = register_session(&state).await;
            let mut msg = body.clone();
            msg["session_id"] = json!(sid);
            let status = post_to_relay(&state, &msg).await;
            assert_eq!(
                status, 200,
                "{rel}：已注册会话的普通消息必须被接受（HTTP 200），实际 {status}"
            );
        }
    }
}

/// 需要转发给浏览器的类型，回放时必须真的转发出去。
///
/// 判据来自 relay 的 `broadcast_types`：这些类型的消息要落到浏览器 SSE 端。
/// 只挑**不依赖外部副作用**的那几类（终端输出、会话状态、桌面控制事件），
/// 它们走的是纯广播路径，回放一次就能观测到投递。
#[tokio::test]
async fn test_replay_broadcast_types_reach_browser() {
    // 与 relay `route_agent_message` 里 `broadcast_types` 的纯广播子集对应。
    // 刻意不含 fs:result / mcp:result（它们另走 oneshot / 下载 sink 分支，
    // 在无 pending 时本就不该广播），也不含 desktop:video（走独立 fan-out 流）。
    let broadcast_cases = [
        "terminal:output",
        "session:users",
        "session:tab_list",
        "session:tab_switched",
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
        "desktop:p2p-answer",
        "desktop:p2p-state",
    ];

    for version in ALL_VERSIONS {
        for ty in broadcast_cases {
            let want = ty;
            let name = list_fixtures(version, AGENT_TO_RELAY)
                .into_iter()
                .find(|n| {
                    !is_register_fixture(n)
                        && n.ends_with(".json")
                        && read_fixture_json(&fixture_rel_path(version, AGENT_TO_RELAY, n))
                            ["type"]
                            .as_str()
                            == Some(want)
                })
                .unwrap_or_else(|| {
                    panic!("{version}/{AGENT_TO_RELAY} 里找不到 {ty} 的夹具")
                });
            let rel = fixture_rel_path(version, AGENT_TO_RELAY, &name);

            let state = make_state();
            let (sid, _agent_rx) = register_session(&state).await;
            let mut browser_rx = subscribe_browser(&state, &sid).await;
            let mut msg = read_fixture_json(&rel);
            msg["session_id"] = json!(sid);

            let status = post_to_relay(&state, &msg).await;
            assert_eq!(status, 200, "{rel}：回放必须被接受");
            let got = tokio::time::timeout(std::time::Duration::from_millis(500), browser_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{rel}：{ty} 必须转发给浏览器，但 500ms 内没收到"));
            let forwarded: Value = serde_json::from_str(&got.expect("浏览器通道不能关闭"))
                .expect("转发的必须是 JSON");
            assert_eq!(
                forwarded["type"], ty,
                "{rel}：浏览器收到的类型不对"
            );
        }
    }
}

/// v0 夹具里的协议版本必须被记成 0——这是「旧 agent 连新 relay」的核心语义：
/// 缺 `protocol_version` 不是错误，而是「标记出现之前的那一代」。
///
/// 单独一条用例，因为它对应验收标准里「临时让 relay 注册处理把缺
/// protocol_version 的请求当成错误 → v0 回放测试失败」这一项。
#[tokio::test]
async fn test_v0_register_without_protocol_version_is_accepted_as_zero() {
    let rel = fixture_rel_path(V0, AGENT_TO_RELAY, "agent_register__new.json");
    let body = read_fixture_json(&rel);
    assert!(
        body.get("protocol_version").is_none(),
        "v0 注册夹具必须**没有** protocol_version 字段（这就是 v0 的定义）"
    );

    let state = make_state();
    let status = post_to_relay(&state, &body).await;
    assert_eq!(
        status, 200,
        "缺 protocol_version 的注册**绝不能**被当成错误：那是 v0 旧 agent 的正常形态"
    );
    let (sid, info) = state
        .sessions
        .list_sessions()
        .await
        .into_iter()
        .next()
        .expect("必须留下一个会话");
    assert_eq!(info.agent_protocol_version, 0, "v0 必须记成协议版本 0");
    assert!(!sid.is_empty());
}

// ── 2b. 回放：agent 侧 ────────────────────────────────────────────

/// 所有版本的 `relay_to_agent` 夹具都必须能被 agent 的**信封解析**吃下去。
///
/// `RelayClient::recv` 的解析口径就是 `serde_json::from_str::<ProtoMessage>`；
/// 这里用同一个类型，保证「夹具能解析」等于「agent 主循环不会因为这条消息
/// 打 warn 并丢弃」。
#[test]
fn test_replay_relay_to_agent_fixtures_parse_as_messages() {
    for version in ALL_VERSIONS {
        for name in list_fixtures(version, RELAY_TO_AGENT) {
            let rel = fixture_rel_path(version, RELAY_TO_AGENT, &name);
            let text = read_fixture(&rel);
            let msg: crate::proto::Message = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{rel}：agent 必须能解析这条消息，实际失败: {e}"));
            assert!(
                !msg.msg_type.is_empty(),
                "{rel}：解析出来的 type 不能为空"
            );
        }
    }
}

/// 所有版本的 `relay_to_agent` 夹具都必须被 agent 的**分发逻辑**安全处理：
/// 不 panic、类型要么被某个分支消费、要么被当作未知类型忽略。
///
/// agent 的分发是一大段 `match msg.msg_type`，其中绝大多数分支会真的动 PTY /
/// 文件 / 桌面（回放会改磁盘、需要有显示器）。这里用**注册表 + 解析**做等价
/// 断言：类型已登记 ⇔ 有一个 `match` 分支在处理它（`test_proto_registry`
/// 守住注册表是唯一登记处）。再叠加「信封解析成功且进程不 panic」，
/// 就覆盖了「喂给 agent 的消息解析和分发逻辑时不 panic」这条要求。
#[test]
fn test_replay_relay_to_agent_fixtures_are_known_or_ignored() {
    for version in ALL_VERSIONS {
        for name in list_fixtures(version, RELAY_TO_AGENT) {
            let rel = fixture_rel_path(version, RELAY_TO_AGENT, &name);
            let msg: crate::proto::Message =
                serde_json::from_str(&read_fixture(&rel)).expect("信封必须能解析");
            // 已知类型由对应分支处理；未知类型 agent 记 debug 后忽略（不 panic、
            // 不断连）。两种都算「不 panic 地处理完」。
            let known = crate::proto::is_known_relay_type(&msg.msg_type);
            let is_register_response = msg.msg_type == "agent:registered";
            assert!(
                known || is_register_response,
                "{rel}：{} 既不在 RELAY_TO_AGENT_TYPES 也不是注册响应——\
                 它会被 agent 当未知类型静默忽略，说明这条夹具录错了方向或\
                 注册表漏登记",
                msg.msg_type
            );
        }
    }
}

/// v0 注册响应（无新字段）解析后：relay 协议版本 0、按 `desktop_binary` 回退。
#[test]
fn test_replay_v0_register_response_falls_back_to_desktop_binary() {
    let rel = fixture_rel_path(V0, RELAY_TO_AGENT, "agent_registered.json");
    let response = read_fixture_json(&rel);
    assert!(
        response.get("relay_capabilities").is_none(),
        "v0 注册响应必须**没有** relay_capabilities（否则就不是旧 relay 的形态）"
    );
    assert_eq!(
        response["desktop_binary"], true,
        "v0 注册响应必须带 desktop_binary: true——旧 agent 只认它"
    );

    let hs = crate::agent::client::parse_register_handshake(&response)
        .expect("v0 注册响应必须能被新 agent 解析");
    assert_eq!(hs.relay_protocol_version, 0, "v0 响应 → relay 协议版本 0");
    assert_eq!(hs.relay_version, None, "v0 响应 → relay 版本未知");
    assert!(
        hs.desktop_binary,
        "v0 响应没有 relay_capabilities 时，desktop_binary 判定必须回退到 \
         desktop_binary 字段（true）"
    );
    assert_eq!(hs.session_id, "goldensession1");
    assert_eq!(
        hs.tokens,
        vec![
            ("golden-rw-token".to_string(), "rw".to_string()),
            ("golden-ro-token".to_string(), "ro".to_string())
        ]
    );
}

/// v1 注册响应解析后：协议版本 1、relay 版本就位、能力以 relay_capabilities 为准。
#[test]
fn test_replay_v1_register_response_reads_new_fields() {
    let rel = fixture_rel_path(V1, RELAY_TO_AGENT, "agent_registered.json");
    let response = read_fixture_json(&rel);
    let hs = crate::agent::client::parse_register_handshake(&response)
        .expect("v1 注册响应必须能被解析");
    assert_eq!(hs.relay_protocol_version, crate::proto::PROTOCOL_VERSION);
    // 冻结夹具里记录的是录制时的 relay 版本，与当前构建版本无关：
    // 用 CARGO_PKG_VERSION 比对会让每次发版的版本号 bump 都打破本用例。
    assert_eq!(hs.relay_version.as_deref(), response["relay_version"].as_str());
    assert!(hs.relay_version.is_some(), "v1 注册响应必须带 relay_version");
    assert!(hs.desktop_binary, "v1 声明了 desktop:binary-uplink");
    assert_eq!(hs.tokens.len(), 2);
}

/// `agent:upgrade` 的两种格式（新字段 / 旧别名字段）必须解析成同一个任务。
#[test]
fn test_replay_agent_upgrade_two_formats_yield_same_task() {
    let new_form = read_fixture_json(&fixture_rel_path(
        V1,
        RELAY_TO_AGENT,
        "agent_upgrade.json",
    ));
    let legacy = read_fixture_json(&fixture_rel_path(
        V1,
        RELAY_TO_AGENT,
        "agent_upgrade__legacy_alias.json",
    ));

    let a = crate::agent::download::UpgradeTask::from_payload(&new_form["payload"])
        .expect("新格式升级任务必须能解析");
    let b = crate::agent::download::UpgradeTask::from_payload(&legacy["payload"])
        .expect("旧别名格式升级任务必须能解析");

    assert_eq!(a.target_version, b.target_version, "target_version/version 必须同源");
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.filename, b.filename, "filename/url 末段必须同源");
    assert_eq!(a.relay_url, b.relay_url, "relay_url/url 必须同源");
    // 旧格式没有 CDN 概念：老 relay 的语义就是「只从 relay 下载」。
    assert!(b.cdn_base_urls.is_empty(), "旧格式不得凭空得到 CDN 源");
    assert!(!b.allows_cdn(), "旧格式必须不允许走 CDN");
    // 新格式带官方路径与 CDN，但**只有存在 relay 副本**才兼容老 agent。
    assert!(a.allows_cdn());
    assert!(a.relay_fallback && a.relay_url.is_some());
}

/// 所有版本的升级任务夹具都必须能解析（防止将来只按新格式改代码）。
#[test]
fn test_replay_all_agent_upgrade_fixtures_parse() {
    for version in ALL_VERSIONS {
        for name in list_fixtures(version, RELAY_TO_AGENT) {
            if !name.starts_with("agent_upgrade") {
                continue;
            }
            let rel = fixture_rel_path(version, RELAY_TO_AGENT, &name);
            let msg = read_fixture_json(&rel);
            assert!(
                crate::agent::download::UpgradeTask::from_payload(&msg["payload"]).is_some(),
                "{rel}：升级任务必须能解析，否则该版本 relay 下发升级时设备会卡住不动"
            );
        }
    }
}

/// 未知类型被忽略（不 panic、不产生副作用）——两侧一致的行为约定。
#[test]
fn test_replay_unknown_type_is_ignored() {
    let unknown = json!({
        "type": "some:future-message",
        "session_id": "goldensession1",
        "payload": {"whatever": 1}
    });
    // 信封仍然能解析（未知类型不是解析错误）。
    let msg: crate::proto::Message =
        serde_json::from_str(&unknown.to_string()).expect("未知类型也必须能解析出信封");
    assert_eq!(msg.msg_type, "some:future-message");
    assert!(!crate::proto::is_known_relay_type(&msg.msg_type));
    assert!(!crate::proto::is_known_agent_type(&msg.msg_type));
    // 未知字段同样被忽略。
    let with_extra = json!({
        "type": "ping",
        "session_id": "s1",
        "payload": {},
        "future_field": {"nested": true}
    });
    let msg: crate::proto::Message =
        serde_json::from_str(&with_extra.to_string()).expect("未知字段必须被忽略");
    assert_eq!(msg.msg_type, "ping");
}

/// `desktop:video` 的二进制帧夹具必须能被真实解码器解开（9 字节头格式）。
#[test]
fn test_replay_binary_video_frame_fixture() {
    let rel = fixture_rel_path(V1, AGENT_TO_RELAY, "desktop_video__bin_frag.bin");
    let full = format!("{FIXTURE_DIR}/{rel}");
    let bytes = std::fs::read(&full).expect("二进制帧夹具必须存在");
    assert!(
        bytes.len() > crate::proto::BIN_FRAME_HEADER_LEN,
        "帧必须长于 9 字节头"
    );
    let (flags, seq, payload) = crate::proto::decode_bin_frame(&bytes)
        .expect("二进制帧夹具必须能被 decode_bin_frame 解开");
    assert_eq!(flags, crate::proto::BIN_FRAME_FLAG_KEY, "样本是关键帧");
    assert_eq!(seq, 7);
    assert!(!payload.is_empty(), "载荷不能为空");
    // 解码出的载荷必须与编码前一致（编码器/解码器契约）。
    let reencoded = crate::proto::encode_bin_frame(flags, seq, payload);
    assert_eq!(reencoded, bytes, "编解码必须往返一致");
}

// ── 3. 输出契约 ────────────────────────────────────────────────────
//
// 这些断言跑的是**当前代码的真实构造函数**，不是夹具文件。夹具只用来记录
// 历史，契约测试用来防止将来的改动删掉旧对端依赖的字段。

/// 断言一个 JSON 对象含有全部必需键，失败时列出缺了哪些。
fn assert_has_fields(v: &Value, required: &[&str], what: &str) {
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("{what} 必须是 JSON 对象，实际 {v}"));
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    assert!(
        missing.is_empty(),
        "{what} 缺少旧版本对端要求的字段：{missing:?}\n\
         实际字段：{:?}\n\
         这些字段一旦删掉，旧 agent / 旧 relay 会静默坏掉——只允许新增，\
         不允许删除。清单见 docs/protocol.md §6 与本文件 CONTRACT_* 常量。",
        {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            keys
        }
    );
}

/// relay 的 `agent:registered` 响应必须包含全部契约字段，且
/// `desktop_binary` 仍为 `true`（v0 旧 agent 的唯一判据）。
#[test]
fn test_contract_agent_registered_keeps_legacy_fields() {
    let resp = crate::relay::ws::build_register_response(
        "goldensession1",
        false,
        vec![json!({"token": "t-rw", "permission": "rw"})],
    );
    assert_has_fields(&resp, CONTRACT_AGENT_REGISTERED, "agent:registered 响应");
    assert_has_fields(
        &resp["payload"],
        CONTRACT_AGENT_REGISTERED_PAYLOAD,
        "agent:registered.payload",
    );
    assert_eq!(
        resp["desktop_binary"], true,
        "desktop_binary 不能删也不能改成 false——v0 旧 agent 靠它判断二进制上行"
    );
    assert_eq!(resp["type"], "agent:registered");
    assert_eq!(resp["protocol_version"], crate::proto::PROTOCOL_VERSION);
    assert_eq!(resp["relay_version"], env!("CARGO_PKG_VERSION"));
    let caps = resp["relay_capabilities"]
        .as_array()
        .expect("relay_capabilities 必须是数组");
    assert!(
        caps.iter()
            .any(|c| c.as_str() == Some(crate::proto::CAP_DESKTOP_BINARY_UPLINK)),
        "relay_capabilities 必须包含 desktop:binary-uplink，实际 {caps:?}"
    );
    // tokens 仍是老格式（数组，每项 token + permission）。
    let tokens = resp["payload"]["tokens"].as_array().expect("tokens 是数组");
    assert!(!tokens.is_empty());
    for t in tokens {
        assert!(t.get("token").is_some(), "token 条目必须有 token");
        assert!(t.get("permission").is_some(), "token 条目必须有 permission");
    }
}

/// relay 的 `agent:upgrade.payload` 必须同时带新字段与旧别名字段。
#[test]
fn test_contract_agent_upgrade_keeps_alias_fields() {
    let task = super::gen::sample_upgrade_task();
    let msg = crate::relay::admin::build_upgrade_message("goldensession1", &task);
    assert_eq!(msg["type"], "agent:upgrade");
    assert_has_fields(
        &msg["payload"],
        CONTRACT_AGENT_UPGRADE_PAYLOAD,
        "agent:upgrade.payload",
    );
    // 别名字段必须与主字段同值——旧 agent 只读别名。
    assert_eq!(msg["payload"]["version"], msg["payload"]["target_version"]);
    assert_eq!(msg["payload"]["url"], msg["payload"]["relay_url"]);
    // 旧 agent 必须能消费这个任务（有 relay 副本）。
    assert!(
        task.legacy_compatible(),
        "带 relay_url 的任务必须对旧 agent 可用"
    );
    // 新 agent 也必须能解析同一份 payload。
    assert!(
        crate::agent::download::UpgradeTask::from_payload(&msg["payload"]).is_some(),
        "新 agent 必须能解析 relay 自己发出的升级任务"
    );
}

/// relay 的 `mcp:exec.payload` 必须带存量 agent 必读的三个键。
#[test]
fn test_contract_mcp_exec_keeps_required_fields() {
    let msg = crate::relay::mcp::build_mcp_exec_message(
        "goldensession1",
        "uname -a",
        30_000,
        None,
        None,
        "req-1",
    );
    assert_eq!(msg.msg_type, "mcp:exec");
    assert_has_fields(&msg.payload, CONTRACT_MCP_EXEC_PAYLOAD, "mcp:exec.payload");
    assert_eq!(msg.payload["cmd"], "uname -a");
    assert_eq!(msg.payload["timeout_ms"], 30_000);
    assert_eq!(msg.payload["_mcp_request_id"], "req-1");
}

/// relay 的 `fs:read.payload` 必须带 `path` 与关联 id；Range 是可选增量。
#[test]
fn test_contract_fs_read_keeps_required_fields() {
    let plain = crate::relay::file_transfer::build_fs_read_message(
        "goldensession1",
        "/home/golden/a.log",
        "c-1",
        None,
    );
    assert_eq!(plain["type"], "fs:read");
    assert_has_fields(&plain["payload"], CONTRACT_FS_READ_PAYLOAD, "fs:read.payload");
    assert!(
        plain["payload"].get("offset").is_none(),
        "非 Range 请求不得带 offset（老 agent 忽略它，但语义上不该出现）"
    );

    let ranged = crate::relay::file_transfer::build_fs_read_message(
        "goldensession1",
        "/home/golden/a.log",
        "c-2",
        Some((100, Some(199))),
    );
    assert_has_fields(&ranged["payload"], CONTRACT_FS_READ_PAYLOAD, "fs:read.payload(range)");
    assert_eq!(ranged["payload"]["offset"], 100);
    assert_eq!(ranged["payload"]["limit"], 100, "limit = end - start + 1");
}

/// agent 的 `agent:register` 必须带全部契约字段（含协议标记），三个变体一致。
#[test]
fn test_contract_agent_register_keeps_required_fields() {
    let cases: Vec<(&str, Value)> = vec![
        (
            "新注册",
            crate::agent::client::build_register_message(
                Some("k".to_string()),
                "rw",
                Some("goldensession1"),
                None,
                None,
                &super::gen::sample_device(),
            ),
        ),
        (
            "带 tokens 重连",
            crate::agent::client::build_register_message(
                None,
                "rw",
                Some("goldensession1"),
                Some(&[("t".to_string(), "rw".to_string())]),
                Some("192.168.1.5:43210".to_string()),
                &super::gen::sample_device(),
            ),
        ),
    ];
    for (label, msg) in cases {
        assert_eq!(msg["type"], "agent:register", "{label}");
        assert_has_fields(&msg, CONTRACT_AGENT_REGISTER, &format!("agent:register（{label}）"));
        assert_eq!(
            msg["protocol_version"],
            crate::proto::PROTOCOL_VERSION,
            "{label}：注册消息必须带当前协议版本"
        );
        assert_eq!(msg["agent_version"], env!("CARGO_PKG_VERSION"));
        assert!(msg["device"].is_object(), "{label}：device 必须是对象");
        assert!(
            msg["capabilities"].as_array().is_some_and(|a| !a.is_empty()),
            "{label}：capabilities 必须是非空数组"
        );
    }
}

/// 回滚重绑变体（`update_apply::bind_identity`）也必须带协议标记——它是
/// 另一条独立构造路径，最容易在改动时被漏掉。
#[test]
fn test_contract_rollback_rebind_carries_protocol_version() {
    let rel = fixture_rel_path(V1, AGENT_TO_RELAY, "agent_register__rollback_rebind.json");
    let msg = read_fixture_json(&rel);
    assert_eq!(msg["type"], "agent:register");
    assert_eq!(
        msg["protocol_version"],
        crate::proto::PROTOCOL_VERSION,
        "回滚重绑消息必须带 protocol_version（否则 relay 会把升级回滚后的设备\
         误记成 v0 旧代次）"
    );
    for absent in ["capabilities", "device", "lan_addr"] {
        assert!(
            msg.get(absent).is_none(),
            "回滚重绑故意不带 {absent}：relay 侧对这三项是「带了才覆盖」，\
             缺省即保留会话上已有的值（MYS-2553 3c）"
        );
    }
    assert!(msg["tokens"].is_array(), "回滚重绑必须带缓存 tokens");
}

/// 契约清单必须与 `docs/protocol.md` §6 的表格一致——文档漏写或多写都算失败。
///
/// 用文本比对而不是人工 review：文档和常量分处两地，只有机器核对才不会漂。
#[test]
fn test_contract_field_lists_match_protocol_doc() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/protocol.md");
    let doc = std::fs::read_to_string(path).expect("docs/protocol.md 必须存在");

    for (what, fields) in [
        ("agent:registered", CONTRACT_AGENT_REGISTERED),
        ("agent:upgrade", CONTRACT_AGENT_UPGRADE_PAYLOAD),
        ("mcp:exec", CONTRACT_MCP_EXEC_PAYLOAD),
        ("fs:read", CONTRACT_FS_READ_PAYLOAD),
        ("agent:register", CONTRACT_AGENT_REGISTER),
    ] {
        for f in fields {
            assert!(
                doc.contains(&format!("`{f}`")),
                "docs/protocol.md 的输出契约一节没有提到 {what} 的字段 `{f}`\
                 ——文档与测试清单必须同步（§6 字段清单）"
            );
        }
    }
}

// ── 生成器入口与统计 ──────────────────────────────────────────────

/// 重新生成 `v1/` 夹具与冻结清单（`v0/` 不动）。
///
/// 只在显式要求时跑：
///
/// ```text
/// REGEN_FIXTURES=1 cargo test --lib proto::fixtures::tests::regen_fixtures -- --ignored --nocapture
/// ```
///
/// 双保险：`#[ignore]` 让普通 `cargo test` 不写盘；环境变量 `REGEN_FIXTURES`
/// 防止加 `--ignored` 连跑整个测试集时无意中改写夹具。
#[test]
#[ignore = "只在显式重新生成夹具时运行，会写盘"]
fn regen_fixtures() {
    assert_eq!(
        std::env::var("REGEN_FIXTURES").as_deref(),
        Ok("1"),
        "重新生成会重写 {V1}/ 与 {FROZEN_FILE}。确认要这么做请设置 \
         REGEN_FIXTURES=1 再跑"
    );
    let (n, _) = super::gen::regen_v1();
    println!("冻结清单共 {n} 条。");
    print_census();
}

/// 夹具统计（交付里要的数字，也是给人看的活文档）：
/// `cargo test --lib proto::fixtures::tests::fixture_census -- --nocapture`
#[test]
fn fixture_census() {
    assert!(print_census() > 0, "夹具总数为 0 —— 生成器没有产出任何夹具");
}

/// 打印「版本 × 方向」的夹具数量，返回总数。
fn print_census() -> usize {
    let mut total = 0;
    println!("夹具数量（版本 × 方向）：");
    for version in ALL_VERSIONS {
        let mut sub = 0;
        for direction in [AGENT_TO_RELAY, RELAY_TO_AGENT] {
            let n = count_fixtures(version, direction);
            sub += n;
            println!("  {version}/{direction}: {n}");
        }
        println!("  {version} 小计: {sub}");
        total += sub;
    }
    println!("合计: {total}");
    total
}

/// 所有版本的 `relay_to_agent` 夹具的 **payload** 都必须能被 agent 分发分支
/// 真正消费：凡是分发路径上有独立解析函数的类型（鼠标/键盘注入、升级任务、
/// 注册响应），直接调那个函数；其余类型按分支里的键读取方式逐字段取值，
/// 断言取到了夹具里写的东西（而不是 `unwrap_or` 的兜底默认值）。
///
/// 为什么不用「跑整条 `match`」：agent 的分发是一段内联在 `run_session` 里的
/// 巨型 `match`，绝大多数分支会真的动 PTY / 磁盘 / 显示器（`terminal:input`
/// 要写 PTY、`fs:write` 要落盘、`desktop:start` 要开抓屏）。回放它们不是回归
/// 测试而是集成测试，且在 CI 里没有显示器可用。这里换一个等价且可重复的口径：
/// **每条夹具的 payload 都过了它所属分支的解析/取值代码**，坏字段会在这一步
/// 暴露（例如把 `desktop:key` 的 `down` 写成字符串，`parse_key` 直接返回 None）。
#[test]
fn test_replay_relay_to_agent_payloads_are_consumable() {
    for version in ALL_VERSIONS {
        for name in list_fixtures(version, RELAY_TO_AGENT) {
            let rel = fixture_rel_path(version, RELAY_TO_AGENT, &name);
            let msg = read_fixture_json(&rel);
            let ty = msg["type"].as_str().expect("夹具必须有 type");
            let p = &msg["payload"];

            // 每个分支都要能取到这些键（缺了业务就静默走默认值）。
            let required: &[&str] = match ty {
                "agent:registered" => continue, // 由注册响应专项用例覆盖
                "agent:upgrade" => continue,    // 由升级任务专项用例覆盖
                "terminal:input" => &["data"],
                "terminal:resize" => &["cols", "rows"],
                "session:join" | "session:leave" => &["user_id", "permission"],
                "session:tab_switch" | "session:tab_close" => &["tab_id"],
                "session:tab_create" => &[],
                "fs:list" | "fs:delete" | "fs:mkdir" => &["path"],
                "fs:read" => &["path"],
                "fs:read_cancel" => &["_mcp_request_id"],
                "fs:write" => &["path", "content"],
                "fs:upload" => &[],
                "fs:rename" => &["from", "to"],
                "mcp:exec" => &["cmd"],
                "mcp:exec_start" => &["cmd"],
                "mcp:exec_input" => &["exec_id", "data_b64"],
                "mcp:exec_close" => &["exec_id"],
                "mcp:exec_list" => &[],
                "desktop:start" | "desktop:stop" | "desktop:reqkey" => &[],
                "desktop:codec" => &["codec"],
                "desktop:quality" => &["quality"],
                "desktop:bitrate" => &[], // kbps / bps 二选一
                "desktop:gray" => &["enabled"],
                "desktop:test-delay" => &["seq", "t0"],
                "desktop:mouse" => &["type"],
                "desktop:key" => &["code", "down"],
                "desktop:clipboard:get" => &[],
                "desktop:clipboard:set" => &["text"],
                "desktop:select-display" => &[],
                "desktop:qos" => &["delay_ms"],
                "desktop:congested" => &["dropped"],
                "desktop:p2p-offer" => &["sdp"],
                "desktop:p2p-candidate" => &["candidate"],
                "web:request" => &["request_id", "url"],
                "web:cancel" => &["request_id"],
                "tcp:open" => &["conn_id", "host", "port"],
                "tcp:data" => &["conn_id", "seq", "data"],
                "tcp:ack" => &["conn_id", "bytes"],
                "tcp:eof" | "tcp:close" => &["conn_id"],
                other => panic!("{rel}：新增的 relay→agent 类型 {other} 没有登记到\
                                 这条 payload 契约里——补上它必须带的键"),
            };
            for k in required {
                assert!(
                    p.get(*k).is_some(),
                    "{rel}：分发分支要读 payload[{k:?}]，夹具里却没有——\
                     这条夹具不能证明该类型可用"
                );
            }

            // 真实解析函数（分发路径上真的调用的那几个）。
            match ty {
                "mcp:exec" => {
                    assert!(p["cmd"].as_str().is_some_and(|s| !s.is_empty()));
                    assert!(p["timeout_ms"].as_u64().is_some(), "timeout_ms 是整数");
                }
                "tcp:open" => {
                    assert!(p["port"].as_u64().is_some(), "port 是整数");
                }
                "desktop:bitrate" => assert!(
                    p.get("kbps").and_then(Value::as_u64).is_some()
                        || p.get("bps").and_then(Value::as_u64).is_some(),
                    "{rel}：desktop:bitrate 必须有 kbps 或 bps"
                ),
                _ => {}
            }
        }
    }
}

/// 鼠标/键盘注入：夹具必须能被 `desktop/input.rs` 的真实解析器接受。
///
/// 这两个函数就是 `handle_mouse` / `handle_key` 里的那一步；解析返回 `None`
/// 时 agent **静默丢弃**输入（用户看到的是「键鼠没反应」），所以夹具必须
/// 走通这条路径。
#[cfg(feature = "desktop")]
#[test]
fn test_replay_desktop_input_fixtures_parse() {
    for version in ALL_VERSIONS {
        for (fixture, kind) in [("desktop_mouse.json", "mouse"), ("desktop_key.json", "key")] {
            let rel = fixture_rel_path(version, RELAY_TO_AGENT, fixture);
            let msg = read_fixture_json(&rel);
            if kind == "mouse" {
                assert!(
                    crate::agent::desktop::input::parse_mouse(&msg["payload"]).is_some(),
                    "{rel}：parse_mouse 必须能解析这条夹具，否则真实浏览器操作会被静默丢弃"
                );
            } else {
                assert!(
                    crate::agent::desktop::input::parse_key(&msg["payload"]).is_some(),
                    "{rel}：parse_key 必须能解析这条夹具"
                );
            }
        }
    }
}
